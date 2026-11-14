use crate::config;
use crate::error::AppError;

// X shortens every URL to a t.co link of this weight.
const LINK_URL_WEIGHT: usize = 23;

const LINK_LABEL_SEPARATOR: &str = ": ";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadNumbering {
    AllPosts,
    FirstPost,
}

#[derive(Debug)]
pub struct ThreadSettings {
    pub post_limit: usize,
    pub max_posts: usize,
    pub numbering: ThreadNumbering,
    pub overflow_label: Option<String>,
    pub source_label: Option<String>,
}

enum Overflow {
    Absent,
    Present,
}

struct GreedySplit {
    post_texts: Vec<String>,
    overflow: Overflow,
}

struct PostCut<'a> {
    post_text: &'a str,
    remainder: &'a str,
}

pub fn weighted_length(text: &str) -> usize {
    text.chars().map(character_weight).sum()
}

// Ranges from X's weighted-length rule.
fn character_weight(character: char) -> usize {
    let code_point: u32 = character as u32;
    let weighs_one: bool = matches!(code_point, 0..=4351 | 8192..=8205 | 8208..=8223 | 8242..=8247);

    if weighs_one { 1 } else { 2 }
}

pub fn split_post_text(text: &str, source_url: Option<&str>, settings: &ThreadSettings) -> Result<Vec<String>, AppError> {
    let trimmed_text: &str = text.trim_matches(is_split_whitespace);

    let text_split: GreedySplit = split_text_posts(trimmed_text, settings)?;

    let link_post: Option<String> = match text_split.overflow {
        Overflow::Present => Some(compose_overflow_post(source_url, settings)?),
        Overflow::Absent => compose_source_post(source_url, settings),
    };

    let mut post_texts: Vec<String> = text_split.post_texts;
    post_texts.extend(link_post);

    Ok(post_texts)
}

fn split_text_posts(trimmed_text: &str, settings: &ThreadSettings) -> Result<GreedySplit, AppError> {
    if weighted_length(trimmed_text) <= settings.post_limit {
        return Ok(GreedySplit {
            post_texts: vec![trimmed_text.to_string()],
            overflow: Overflow::Absent,
        });
    }

    split_at_stable_post_total(trimmed_text, settings)
}

fn split_at_stable_post_total(text: &str, settings: &ThreadSettings) -> Result<GreedySplit, AppError> {
    let mut assumed_post_total: usize = 1;

    loop {
        let greedy_split: GreedySplit = split_greedily(text, settings, assumed_post_total)?;

        let resulting_post_total: usize = match greedy_split.overflow {
            Overflow::Absent => greedy_split.post_texts.len(),
            Overflow::Present => settings.max_posts,
        };

        if resulting_post_total == assumed_post_total {
            return Ok(greedy_split);
        }

        assumed_post_total = resulting_post_total;
    }
}

fn split_greedily(text: &str, settings: &ThreadSettings, post_total: usize) -> Result<GreedySplit, AppError> {
    let mut post_texts: Vec<String> = Vec::new();
    let mut remaining: &str = text;

    while !remaining.is_empty() {
        if post_texts.len() == settings.max_posts {
            return Ok(GreedySplit {
                post_texts,
                overflow: Overflow::Present,
            });
        }

        let position: usize = post_texts.len() + 1;
        let suffix: String = numbering_suffix(position, post_total, settings.numbering);
        let capacity: usize = settings.post_limit.saturating_sub(weighted_length(&suffix));

        let post_cut: PostCut<'_> = cut_post(remaining, capacity)?;

        post_texts.push(format!("{}{}", post_cut.post_text, suffix));
        remaining = post_cut.remainder;
    }

    Ok(GreedySplit {
        post_texts,
        overflow: Overflow::Absent,
    })
}

fn numbering_suffix(position: usize, post_total: usize, numbering: ThreadNumbering) -> String {
    let numbered: bool = match numbering {
        ThreadNumbering::AllPosts => true,
        ThreadNumbering::FirstPost => position == 1,
    };

    if !numbered {
        return String::new();
    }

    format!(" {}/{}", position, post_total)
}

fn cut_post(remaining: &str, capacity: usize) -> Result<PostCut<'_>, AppError> {
    if weighted_length(remaining) <= capacity {
        return Ok(PostCut {
            post_text: remaining,
            remainder: "",
        });
    }

    let Some(cut_index) = find_last_fitting_whitespace(remaining, capacity) else {
        return Err(word_too_long_error(remaining, capacity));
    };

    Ok(PostCut {
        post_text: remaining[..cut_index].trim_end_matches(is_split_whitespace),
        remainder: remaining[cut_index..].trim_start_matches(is_split_whitespace),
    })
}

fn find_last_fitting_whitespace(text: &str, capacity: usize) -> Option<usize> {
    let mut preceding_weight: usize = 0;
    let mut last_fitting_whitespace: Option<usize> = None;

    for (byte_index, character) in text.char_indices() {
        if preceding_weight > capacity {
            break;
        }

        if is_split_whitespace(character) {
            last_fitting_whitespace = Some(byte_index);
        }

        preceding_weight += character_weight(character);
    }

    last_fitting_whitespace
}

fn word_too_long_error(remaining: &str, capacity: usize) -> AppError {
    let word: &str = remaining.split(is_split_whitespace).next().unwrap_or(remaining);

    AppError::new(&format!(
        "word '{}' weighs {}, more than the {} a thread post can hold",
        word,
        weighted_length(word),
        capacity,
    ))
}

fn compose_overflow_post(source_url: Option<&str>, settings: &ThreadSettings) -> Result<String, AppError> {
    let Some(source_url) = source_url else {
        return Err(AppError::new(&format!(
            "text needs more than {} thread posts but the record has no source_url",
            settings.max_posts,
        )));
    };

    let Some(overflow_label) = settings.overflow_label.as_deref() else {
        return Err(AppError::new(&format!(
            "text needs more than {} thread posts but the account has no {}",
            settings.max_posts,
            config::THREAD_OVERFLOW_LABEL_KEY,
        )));
    };

    Ok(compose_link_post(overflow_label, source_url))
}

fn compose_source_post(source_url: Option<&str>, settings: &ThreadSettings) -> Option<String> {
    let source_label: &str = settings.source_label.as_deref()?;
    let source_url: &str = source_url?;

    Some(compose_link_post(source_label, source_url))
}

fn compose_link_post(label: &str, url: &str) -> String {
    format!("{}{}{}", label, LINK_LABEL_SEPARATOR, url)
}

pub fn link_post_weight(label: &str) -> usize {
    weighted_length(label) + weighted_length(LINK_LABEL_SEPARATOR) + LINK_URL_WEIGHT
}

fn is_split_whitespace(character: char) -> bool {
    character == ' ' || character == '\n'
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE_URL: &str = "https://example.com/full-text";

    fn create_settings(post_limit: usize, max_posts: usize, numbering: ThreadNumbering) -> ThreadSettings {
        ThreadSettings {
            post_limit,
            max_posts,
            numbering,
            overflow_label: Some("Full text".to_string()),
            source_label: Some("Source".to_string()),
        }
    }

    fn create_numbered_words(word_count: usize) -> String {
        let words: Vec<String> = (1..=word_count).map(|index| format!("w{:03}", index)).collect();

        words.join(" ")
    }

    #[test]
    fn split_post_text_adds_no_source_post_to_a_record_without_a_source_url() {
        let settings: ThreadSettings = create_settings(280, 9, ThreadNumbering::AllPosts);

        let post_texts: Vec<String> = split_post_text("Short text.", None, &settings).unwrap();

        assert_eq!(post_texts, vec!["Short text.".to_string()]);
    }

    #[test]
    fn split_post_text_appends_the_source_post_to_an_unnumbered_single_post() {
        let settings: ThreadSettings = create_settings(280, 9, ThreadNumbering::AllPosts);

        let post_texts: Vec<String> = split_post_text("Short text.", Some(SOURCE_URL), &settings).unwrap();

        assert_eq!(post_texts, vec!["Short text.".to_string(), format!("Source: {}", SOURCE_URL)]);
    }

    #[test]
    fn split_post_text_appends_an_unnumbered_source_post_after_the_numbered_text_posts() {
        let settings: ThreadSettings = create_settings(30, 9, ThreadNumbering::AllPosts);

        let post_texts: Vec<String> = split_post_text(&create_numbered_words(20), Some(SOURCE_URL), &settings).unwrap();
        let text_post_total: usize = post_texts.len() - 1;
        let last_text_post_suffix: String = format!(" {}/{}", text_post_total, text_post_total);

        assert!(post_texts[text_post_total - 1].ends_with(&last_text_post_suffix));
        assert_eq!(post_texts[text_post_total], format!("Source: {}", SOURCE_URL));
    }

    #[test]
    fn split_post_text_trims_whitespace_before_measuring() {
        let settings: ThreadSettings = ThreadSettings {
            post_limit: 20,
            max_posts: 9,
            numbering: ThreadNumbering::AllPosts,
            overflow_label: None,
            source_label: None,
        };
        let padded_text: String = format!("  Short text.{}", "\n".repeat(20));

        let post_texts: Vec<String> = split_post_text(&padded_text, None, &settings).unwrap();

        assert_eq!(post_texts, vec!["Short text.".to_string()]);
    }

    #[test]
    fn weighted_length_counts_em_dash_and_curly_quotes_as_one_and_emoji_as_two() {
        assert_eq!(weighted_length("\u{2014}\u{201C}\u{201D}\u{2018}\u{2019}"), 5);
        assert_eq!(weighted_length("\u{1F600}"), 2);
    }

    #[test]
    fn split_post_text_numbers_every_post_within_the_limit_under_all_posts() {
        let settings: ThreadSettings = create_settings(30, 9, ThreadNumbering::AllPosts);

        let post_texts: Vec<String> = split_post_text(&create_numbered_words(20), None, &settings).unwrap();
        let post_total: usize = post_texts.len();

        for (index, post_text) in post_texts.iter().enumerate() {
            assert!(post_text.ends_with(&format!(" {}/{}", index + 1, post_total)));
            assert!(weighted_length(post_text) <= settings.post_limit);
        }
    }

    #[test]
    fn split_post_text_numbers_only_the_first_post_under_first_post() {
        let settings: ThreadSettings = create_settings(30, 9, ThreadNumbering::FirstPost);

        let post_texts: Vec<String> = split_post_text(&create_numbered_words(20), None, &settings).unwrap();
        let first_post_suffix: String = format!(" 1/{}", post_texts.len());

        assert!(post_texts[0].ends_with(&first_post_suffix));
        assert!(post_texts[1..].iter().all(|post_text| !post_text.contains('/')));
    }

    #[test]
    fn split_post_text_drops_the_cut_whitespace_and_keeps_internal_paragraph_breaks() {
        let settings: ThreadSettings = create_settings(20, 9, ThreadNumbering::AllPosts);

        let post_texts: Vec<String> = split_post_text("one\n\ntwo three four five six", None, &settings).unwrap();

        assert_eq!(post_texts, vec!["one\n\ntwo three 1/2".to_string(), "four five six 2/2".to_string()]);
    }

    #[test]
    fn split_post_text_recounts_when_the_post_total_reaches_two_digits() {
        let settings: ThreadSettings = create_settings(13, 30, ThreadNumbering::AllPosts);

        let post_texts: Vec<String> = split_post_text(&create_numbered_words(19), None, &settings).unwrap();

        assert_eq!(post_texts.len(), 19);
    }

    #[test]
    fn split_post_text_appends_the_overflow_post_instead_of_the_source_post_after_max_posts() {
        let settings: ThreadSettings = create_settings(40, 3, ThreadNumbering::AllPosts);

        let post_texts: Vec<String> = split_post_text(&create_numbered_words(40), Some(SOURCE_URL), &settings).unwrap();

        assert_eq!(post_texts.len(), 4);
        assert!(post_texts[2].ends_with(" 3/3"));
        assert_eq!(post_texts[3], format!("Full text: {}", SOURCE_URL));
    }

    #[test]
    fn split_post_text_rejects_overflow_without_a_source_url() {
        let settings: ThreadSettings = create_settings(40, 3, ThreadNumbering::AllPosts);

        assert!(split_post_text(&create_numbered_words(40), None, &settings).is_err());
    }

    #[test]
    fn split_post_text_rejects_overflow_without_an_overflow_label() {
        let settings: ThreadSettings = ThreadSettings {
            post_limit: 40,
            max_posts: 3,
            numbering: ThreadNumbering::AllPosts,
            overflow_label: None,
            source_label: None,
        };

        assert!(split_post_text(&create_numbered_words(40), Some(SOURCE_URL), &settings).is_err());
    }

    #[test]
    fn split_post_text_rejects_a_word_longer_than_the_post_capacity() {
        let settings: ThreadSettings = create_settings(10, 9, ThreadNumbering::AllPosts);

        assert!(split_post_text("tiny supercalifragilistic", None, &settings).is_err());
    }
}
