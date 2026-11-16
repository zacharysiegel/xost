use std::io::BufRead;
use std::io::Write;

use crate::error::AppError;

pub fn read_line_answer(prompt: &str) -> Result<String, AppError> {
    print!("{}", prompt);
    std::io::stdout().flush()?;

    let mut answer: String = String::new();
    let bytes_read: usize = std::io::stdin().lock().read_line(&mut answer)?;

    if bytes_read == 0 {
        return Err(AppError::new("standard input closed before the prompt was answered"));
    }

    Ok(answer.trim().to_string())
}
