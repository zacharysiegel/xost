use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use chromiumoxide::Browser;
use chromiumoxide::BrowserConfig;
use chromiumoxide::Element;
use chromiumoxide::Page;
use chromiumoxide::browser::BrowserConfigBuilder;
use chromiumoxide::cdp::browser_protocol::emulation::UserAgentBrandVersion;
use chromiumoxide::cdp::browser_protocol::emulation::UserAgentMetadata;
use chromiumoxide::cdp::browser_protocol::input::InsertTextParams;
use chromiumoxide::cdp::browser_protocol::network::Cookie;
use chromiumoxide::cdp::browser_protocol::network::CookieParam;
use chromiumoxide::cdp::browser_protocol::network::EnableParams;
use chromiumoxide::cdp::browser_protocol::network::SetBypassServiceWorkerParams;
use chromiumoxide::cdp::browser_protocol::network::SetUserAgentOverrideParams;
use chromiumoxide::cdp::browser_protocol::network::TimeSinceEpoch;
use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
use futures::StreamExt;

use crate::clock;
use crate::config::Config;
use crate::error::AppError;
use crate::secrets;

const DEFAULT_CHROME_MACOS: &str = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const BROWSER_EXIT_TIMEOUT: Duration = Duration::from_secs(10);
const EDITOR_SETTLE_DELAY: Duration = Duration::from_millis(800);

const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/157.0.0.0 Safari/537.36";

const URL_HOME: &str = "https://x.com/home";
const URL_LOGIN: &str = "https://x.com/";
const URL_COMPOSE: &str = "https://x.com/compose/post";

const SELECTOR_COMPOSE_NAV: &str = "a[data-testid=\"SideNav_NewTweet_Button\"]";
const SELECTOR_LOGIN_USERNAME: &str = "input[name=\"username_or_email\"]";
const SELECTOR_LOGIN_PASSWORD: &str = "input[name=\"password\"]";
const SELECTOR_LOGIN_CHALLENGE: &str = "input[data-testid=\"ocfEnterTextTextInput\"]";
// The compose page also renders the home timeline's inline composer behind the dialog, with the same test ids.
const SELECTOR_COMPOSE_DIALOG: &str = "[role=\"dialog\"]";
const SELECTOR_TWEET_BUTTON: &str = "[role=\"dialog\"] button[data-testid=\"tweetButton\"]";
const SELECTOR_ADD_POST_BUTTON: &str = "[role=\"dialog\"] button[data-testid=\"addButton\"]";
const SELECTOR_LATEST_POST_TIME: &str = "article time";

pub enum PostFailure {
    EnvironmentBeforeCompose(AppError),
    RecordBeforePublish(AppError),
    AfterPublish(AppError),
}

pub struct Session {
    browser: Browser,
    page: Page,
    handler_task: tokio::task::JoinHandle<()>,
}

impl Session {
    pub async fn launch(config: &Config) -> Result<Session, AppError> {
        Session::launch_with_profile(config, &config.profile_dir).await
    }

    pub async fn launch_with_profile(config: &Config, profile_dir: &std::path::Path) -> Result<Session, AppError> {
        std::fs::create_dir_all(profile_dir)?;

        let mut builder: BrowserConfigBuilder = BrowserConfig::builder()
            .user_data_dir(profile_dir)
            .window_size(1280, 900)
            .new_headless_mode()
            .request_timeout(Duration::from_secs(60))
            .arg("--disable-blink-features=AutomationControlled");

        if let Some(chrome_path) = &config.chrome_path {
            builder = builder.chrome_executable(chrome_path);
        }

        let browser_config: BrowserConfig = builder.build().map_err(|message| AppError::new(&message))?;

        let (mut browser, mut handler) = Browser::launch(browser_config).await?;
        let handler_task: tokio::task::JoinHandle<()> = tokio::spawn(async move {
            while handler.next().await.is_some() {}
        });

        let page_preparation: Result<Page, AppError> = prepare_page(&browser).await;

        match page_preparation {
            Ok(page) => Ok(Session {
                browser,
                page,
                handler_task,
            }),
            Err(error) => {
                shut_down_browser(&mut browser, handler_task).await.ok();

                Err(error)
            }
        }
    }

    pub async fn ensure_logged_in(&self, config: &Config) -> Result<(), AppError> {
        self.page.goto(URL_HOME).await?;
        self.page.wait_for_navigation().await?;

        if self.is_logged_in().await {
            return Ok(());
        }

        self.log_in(config).await
    }

    pub async fn diagnose(&self, screenshot_path: &str) -> Result<(), AppError> {
        self.page.goto(URL_HOME).await?;

        tokio::time::sleep(Duration::from_secs(4)).await;

        let current_url: Option<String> = self.page.url().await?;
        let logged_in: bool = self.is_logged_in().await;

        self.page
            .save_screenshot(chromiumoxide::page::ScreenshotParams::default(), screenshot_path)
            .await?;

        println!("url: {:?}", current_url);
        println!("logged in (compose button present): {}", logged_in);
        println!("screenshot: {}", screenshot_path);

        Ok(())
    }

    pub async fn attempt_login(&self, config: &Config, screenshot_path: &str) -> Result<(), AppError> {
        let login_result: Result<(), AppError> = self.log_in(config).await;

        self.page
            .save_screenshot(chromiumoxide::page::ScreenshotParams::default(), screenshot_path)
            .await
            .ok();

        login_result
    }

    pub async fn export_cookies(&self) -> Result<String, AppError> {
        self.page.goto(URL_HOME).await?;
        tokio::time::sleep(Duration::from_secs(3)).await;

        let cookies: Vec<Cookie> = self.page.get_cookies().await?;

        let params: Vec<CookieParam> = cookies
            .into_iter()
            .filter(|cookie| cookie.domain.contains("x.com"))
            .map(|cookie| {
                let mut param: CookieParam = CookieParam::new(cookie.name, cookie.value);
                param.domain = Some(cookie.domain);
                param.path = Some(cookie.path);
                param.secure = Some(cookie.secure);
                param.http_only = Some(cookie.http_only);
                param.same_site = cookie.same_site;
                param.expires = Some(TimeSinceEpoch::new(cookie.expires));

                param
            })
            .collect();

        if params.is_empty() {
            return Err(AppError::new("no x.com cookies found; is this profile logged in?"));
        }

        let json: String = serde_json::to_string_pretty(&params)?;

        Ok(json)
    }

    pub async fn import_cookies(&self, cookies_json: &str) -> Result<(), AppError> {
        let params: Vec<CookieParam> = serde_json::from_str(cookies_json)?;

        self.page.goto("https://x.com/").await?;
        tokio::time::sleep(Duration::from_secs(2)).await;

        self.page.set_cookies(params).await?;

        self.page.goto(URL_HOME).await?;
        tokio::time::sleep(Duration::from_secs(3)).await;

        if !self.is_logged_in().await {
            return Err(AppError::new(
                "imported cookies did not produce a logged-in session; they may be expired or incomplete",
            ));
        }

        Ok(())
    }

    pub async fn already_posted_today(&self, config: &Config) -> Result<bool, AppError> {
        let profile_url: String = format!("https://x.com/{}", config.handle);
        self.page.goto(&profile_url).await?;

        let time_element: Result<Element, AppError> =
            wait_for_element(&self.page, SELECTOR_LATEST_POST_TIME, Duration::from_secs(12)).await;
        let Ok(element) = time_element else {
            return Ok(false);
        };

        let datetime: Option<String> = element.attribute("datetime").await?;
        let Some(iso_timestamp) = datetime else {
            return Ok(false);
        };

        clock::falls_on_today_utc(&iso_timestamp)
    }

    pub async fn post(&self, post_texts: &[String]) -> Result<(), PostFailure> {
        self.open_composer()
            .await
            .map_err(PostFailure::EnvironmentBeforeCompose)?;

        let publish_button: Element = self
            .compose_thread(post_texts)
            .await
            .map_err(PostFailure::RecordBeforePublish)?;

        publish_button
            .click()
            .await
            .map_err(|error| PostFailure::AfterPublish(AppError::from(error)))?;

        self.confirm_posted()
            .await
            .map_err(PostFailure::AfterPublish)?;

        Ok(())
    }

    pub async fn close(mut self) -> Result<(), AppError> {
        shut_down_browser(&mut self.browser, self.handler_task).await
    }

    async fn is_logged_in(&self) -> bool {
        wait_for_element(&self.page, SELECTOR_COMPOSE_NAV, Duration::from_secs(8))
            .await
            .is_ok()
    }

    async fn log_in(&self, config: &Config) -> Result<(), AppError> {
        let password: String = secrets::get_required(config, &config.password_secret)?;
        let totp_seed: Option<String> = secrets::get_optional(config, &config.totp_seed_secret)?;

        self.page.goto(URL_LOGIN).await?;
        tokio::time::sleep(Duration::from_secs(3)).await;

        let username_present: bool =
            self.wait_for_input(SELECTOR_LOGIN_USERNAME, Duration::from_secs(20)).await?;
        if !username_present {
            return Err(AppError::new("login: username field never appeared on the landing page"));
        }

        let username_filled: bool = self.fill_input(SELECTOR_LOGIN_USERNAME, &config.username).await?;
        if !username_filled {
            return Err(AppError::new("login: could not fill the username field"));
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
        self.click_by_text(&["next", "continue", "log in"]).await?;
        tokio::time::sleep(Duration::from_secs(3)).await;

        let password_present: bool =
            self.wait_for_input(SELECTOR_LOGIN_PASSWORD, Duration::from_secs(15)).await?;
        if !password_present {
            return Err(AppError::new(
                "login: the password field never became visible (likely a captcha or an unrecognized login screen)",
            ));
        }

        let password_filled: bool = self.fill_input(SELECTOR_LOGIN_PASSWORD, &password).await?;
        if !password_filled {
            return Err(AppError::new("login: could not fill the password field"));
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
        self.click_by_text(&["log in", "sign in"]).await?;
        tokio::time::sleep(Duration::from_secs(5)).await;

        if let Some(seed) = totp_seed {
            self.submit_totp_if_prompted(&seed).await?;
        }

        self.page.goto(URL_HOME).await?;

        if !self.is_logged_in().await {
            return Err(AppError::new(
                "login did not reach the home timeline; the account needs a verification code (SMS/email 2FA), \
                 or the credentials are wrong. Run `xost login` to sign in manually.",
            ));
        }

        Ok(())
    }

    async fn submit_totp_if_prompted(&self, seed_base32: &str) -> Result<(), AppError> {
        let challenge_input: Result<Element, AppError> =
            wait_for_element(&self.page, SELECTOR_LOGIN_CHALLENGE, Duration::from_secs(10)).await;
        let Ok(input) = challenge_input else {
            return Ok(());
        };

        let code: String = generate_totp_code(seed_base32)?;
        input.click().await?;
        input.type_str(&code).await?;
        input.press_key("Enter").await?;

        Ok(())
    }

    async fn open_composer(&self) -> Result<(), AppError> {
        self.page.goto(URL_COMPOSE).await?;

        let _first_editor: Element =
            wait_for_element(&self.page, &post_editor_selector(0), Duration::from_secs(20)).await?;

        Ok(())
    }

    async fn compose_thread(&self, post_texts: &[String]) -> Result<Element, AppError> {
        for (post_index, post_text) in post_texts.iter().enumerate() {
            if post_index > 0 {
                self.add_thread_post(post_index).await?;
            }

            self.fill_editor(post_index, post_text).await?;
            self.confirm_editor_holds(post_index, post_text).await?;
        }

        let publish_button_enabled: bool = self
            .wait_until_script_true(publish_button_enabled_script()?, Duration::from_secs(15))
            .await?;
        if !publish_button_enabled {
            return Err(AppError::new("the publish button never became enabled; X may weigh the text differently"));
        }

        let publish_button: Element = wait_for_element(&self.page, SELECTOR_TWEET_BUTTON, Duration::from_secs(5)).await?;

        Ok(publish_button)
    }

    async fn add_thread_post(&self, post_index: usize) -> Result<(), AppError> {
        let add_post_button: Element =
            wait_for_element(&self.page, SELECTOR_ADD_POST_BUTTON, Duration::from_secs(10)).await?;
        add_post_button.click().await?;

        let _new_editor: Element =
            wait_for_element(&self.page, &post_editor_selector(post_index), Duration::from_secs(20)).await?;

        // X re-renders a new editor right after creating it.
        tokio::time::sleep(EDITOR_SETTLE_DELAY).await;

        Ok(())
    }

    // Element::type_str fails on any character missing from a US keyboard layout, including "\n".
    async fn fill_editor(&self, post_index: usize, post_text: &str) -> Result<(), AppError> {
        let focused: bool = self.eval_bool(focus_post_editor_script(post_index)?).await?;
        if !focused {
            return Err(AppError::new(&format!("thread post {} has no editor to focus", post_index + 1)));
        }

        for (line_index, line) in post_text.split('\n').enumerate() {
            if line_index > 0 {
                let editor: Element =
                    wait_for_element(&self.page, &post_editor_selector(post_index), Duration::from_secs(5)).await?;
                editor.press_key("Enter").await?;
            }

            if !line.is_empty() {
                self.page.execute(InsertTextParams::new(line)).await?;
            }
        }

        Ok(())
    }

    async fn confirm_editor_holds(&self, post_index: usize, post_text: &str) -> Result<(), AppError> {
        let editor_text: String = self.eval_string(post_editor_text_script(post_index)?).await?;

        let editor_words: Vec<&str> = editor_text.split_whitespace().collect();
        let expected_words: Vec<&str> = post_text.split_whitespace().collect();

        if editor_words != expected_words {
            return Err(AppError::new(&format!(
                "thread post {} does not hold the intended text after typing; editor shows: {}",
                post_index + 1,
                editor_text,
            )));
        }

        Ok(())
    }

    async fn confirm_posted(&self) -> Result<(), AppError> {
        for _attempt in 0..20 {
            let current_url: Option<String> = self.page.url().await?;
            let still_composing: bool = current_url
                .as_deref()
                .map(|url| url.contains("/compose"))
                .unwrap_or(false);

            if !still_composing {
                return Ok(());
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        Err(AppError::new(
            "post submission not confirmed: still on the composer URL past the timeout",
        ))
    }

    async fn fill_input(&self, selector: &str, value: &str) -> Result<bool, AppError> {
        let script: String = format!(
            "(() => {{ const element = document.querySelector({selector}); if (!element) return false; \
             const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set; \
             const previous = element.value; \
             setter.call(element, {value}); \
             if (element._valueTracker) element._valueTracker.setValue(previous); \
             element.dispatchEvent(new Event('input', {{ bubbles: true }})); \
             element.dispatchEvent(new Event('change', {{ bubbles: true }})); return true; }})()",
            selector = serde_json::to_string(selector)?,
            value = serde_json::to_string(value)?,
        );

        self.eval_bool(script).await
    }

    async fn click_by_text(&self, needles: &[&str]) -> Result<bool, AppError> {
        let script: String = format!(
            "(() => {{ const needles = {needles}; \
             const nodes = Array.from(document.querySelectorAll('button, [role=button], a')); \
             for (const node of nodes) {{ const text = (node.textContent || '').trim().toLowerCase(); \
             if (!needles.some(needle => text === needle || text.includes(needle))) continue; \
             if (node.disabled || node.getAttribute('aria-disabled') === 'true') continue; \
             node.click(); return true; }} return false; }})()",
            needles = serde_json::to_string(needles)?,
        );

        self.eval_bool(script).await
    }

    async fn wait_for_input(&self, selector: &str, timeout: Duration) -> Result<bool, AppError> {
        let script: String = format!(
            "(() => {{ const element = document.querySelector({selector}); \
             return !!(element && (element.offsetParent !== null || element.getClientRects().length > 0)); }})()",
            selector = serde_json::to_string(selector)?,
        );

        self.wait_until_script_true(script, timeout).await
    }

    async fn wait_until_script_true(&self, script: String, timeout: Duration) -> Result<bool, AppError> {
        let start: tokio::time::Instant = tokio::time::Instant::now();

        loop {
            if self.eval_bool(script.clone()).await? {
                return Ok(true);
            }

            if start.elapsed() >= timeout {
                return Ok(false);
            }

            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }

    async fn eval_string(&self, script: String) -> Result<String, AppError> {
        let params: EvaluateParams = EvaluateParams::builder()
            .expression(script)
            .return_by_value(true)
            .build()
            .map_err(|message| AppError::new(&message))?;

        let value: String = self.page.evaluate(params).await?.into_value().unwrap_or_default();

        Ok(value)
    }

    async fn eval_bool(&self, script: String) -> Result<bool, AppError> {
        let params: EvaluateParams = EvaluateParams::builder()
            .expression(script)
            .return_by_value(true)
            .build()
            .map_err(|message| AppError::new(&message))?;

        let value: bool = self.page.evaluate(params).await?.into_value().unwrap_or(false);

        Ok(value)
    }
}

async fn wait_for_element(page: &Page, selector: &str, timeout: Duration) -> Result<Element, AppError> {
    let start: tokio::time::Instant = tokio::time::Instant::now();

    loop {
        match page.find_element(selector).await {
            Ok(element) => return Ok(element),
            Err(error) => {
                if start.elapsed() >= timeout {
                    return Err(AppError::from(error));
                }

                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        }
    }
}

fn post_editor_selector(post_index: usize) -> String {
    format!("{} div[data-testid=\"tweetTextarea_{}\"]", SELECTOR_COMPOSE_DIALOG, post_index)
}

// Focus through the DOM, not a mouse click: a click can land on the dialog backdrop and close it.
fn focus_post_editor_script(post_index: usize) -> Result<String, AppError> {
    let script: String = format!(
        "(() => {{ const editor = document.querySelector({selector}); if (!editor) return false; \
         if (!editor.contains(document.activeElement)) editor.focus(); return true; }})()",
        selector = serde_json::to_string(&post_editor_selector(post_index))?,
    );

    Ok(script)
}

fn post_editor_text_script(post_index: usize) -> Result<String, AppError> {
    let script: String = format!(
        "(() => {{ const editor = document.querySelector({selector}); return editor ? editor.innerText : ''; }})()",
        selector = serde_json::to_string(&post_editor_selector(post_index))?,
    );

    Ok(script)
}

fn publish_button_enabled_script() -> Result<String, AppError> {
    let script: String = format!(
        "(() => {{ const element = document.querySelector({selector}); \
         return !!(element && !element.disabled && element.getAttribute('aria-disabled') !== 'true'); }})()",
        selector = serde_json::to_string(SELECTOR_TWEET_BUTTON)?,
    );

    Ok(script)
}

fn generate_totp_code(seed_base32: &str) -> Result<String, AppError> {
    let secret: totp_rs::Secret = totp_rs::Secret::try_from_base32(seed_base32)?;
    let totp: totp_rs::Totp = totp_rs::Builder::new().with_secret(secret).build()?;
    let code: String = totp.generate_current().to_string();

    Ok(code)
}

async fn prepare_page(browser: &Browser) -> Result<Page, AppError> {
    let page: Page = browser.new_page("about:blank").await?;

    page.execute(EnableParams::default()).await?;
    page.execute(SetBypassServiceWorkerParams::new(true)).await?;
    apply_user_agent(&page).await?;

    Ok(page)
}

// Retries relaunch Chrome on the same profile, which fails while the previous process still holds its lock.
async fn shut_down_browser(browser: &mut Browser, handler_task: tokio::task::JoinHandle<()>) -> Result<(), AppError> {
    let close_result: Result<(), AppError> = browser.close().await.map(|_| ()).map_err(AppError::from);

    let graceful_exit: Result<std::io::Result<Option<std::process::ExitStatus>>, tokio::time::error::Elapsed> =
        tokio::time::timeout(BROWSER_EXIT_TIMEOUT, browser.wait()).await;

    let exit_result: std::io::Result<Option<std::process::ExitStatus>> = match graceful_exit {
        Ok(exit_result) => exit_result,
        Err(_elapsed) => {
            let _kill_outcome: Option<std::io::Result<()>> = browser.kill().await;

            browser.wait().await
        }
    };

    handler_task.abort();

    exit_result?;
    close_result
}

async fn apply_user_agent(page: &Page) -> Result<(), AppError> {
    let brands: Vec<UserAgentBrandVersion> = vec![
        UserAgentBrandVersion::new("Not(A:Brand", "99"),
        UserAgentBrandVersion::new("Google Chrome", "157"),
        UserAgentBrandVersion::new("Chromium", "157"),
    ];
    let full_version_list: Vec<UserAgentBrandVersion> = vec![
        UserAgentBrandVersion::new("Not(A:Brand", "99.0.0.0"),
        UserAgentBrandVersion::new("Google Chrome", "157.0.0.0"),
        UserAgentBrandVersion::new("Chromium", "157.0.0.0"),
    ];

    let metadata: UserAgentMetadata = UserAgentMetadata {
        brands: Some(brands),
        full_version_list: Some(full_version_list),
        platform: "macOS".to_string(),
        platform_version: "15.0.0".to_string(),
        architecture: "arm64".to_string(),
        model: String::new(),
        mobile: false,
        bitness: Some("64".to_string()),
        wow64: Some(false),
        form_factors: None,
    };

    let params: SetUserAgentOverrideParams = SetUserAgentOverrideParams {
        user_agent: USER_AGENT.to_string(),
        accept_language: Some("en-US,en;q=0.9".to_string()),
        platform: Some("macOS".to_string()),
        user_agent_metadata: Some(metadata),
    };

    page.execute(params).await?;

    Ok(())
}

pub fn run_manual_login(config: &Config) -> Result<(), AppError> {
    std::fs::create_dir_all(&config.profile_dir)?;

    let chrome: PathBuf = resolve_chrome(config)?;
    let user_data_arg: String = format!("--user-data-dir={}", config.profile_dir.display());

    let status: std::process::ExitStatus = Command::new(&chrome)
        .arg(user_data_arg)
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--password-store=basic")
        .arg("--use-mock-keychain")
        .arg("https://x.com/login")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;

    if !status.success() {
        return Err(AppError::new("the browser exited with an error during manual sign-in"));
    }

    Ok(())
}

fn resolve_chrome(config: &Config) -> Result<PathBuf, AppError> {
    if let Some(path) = &config.chrome_path {
        return Ok(path.clone());
    }

    let default: PathBuf = PathBuf::from(DEFAULT_CHROME_MACOS);
    if default.exists() {
        return Ok(default);
    }

    Err(AppError::new(
        "could not locate Chrome; set `chrome = <path to the Chrome binary>` in the account's config section",
    ))
}
