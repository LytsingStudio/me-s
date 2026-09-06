use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use chrono::{Days, Local, NaiveDate};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::{Result, codex_oauth};

const PROFILE_URL: &str = "https://chatgpt.com/backend-api/wham/profiles/me";
const SECOND_REFRESH: Duration = Duration::from_secs(30 * 60);
const REFRESH_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SAFE_TOKENS: u64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub(crate) struct UsageDay {
    start_date: String,
    tokens: u64,
}

#[derive(Serialize)]
pub(crate) struct UsageSnapshot {
    status: &'static str,
    updated_at: Option<String>,
    range_start: String,
    seven_day_start: String,
    range_end: String,
    days: Vec<UsageDay>,
    total_tokens: Option<u64>,
}

#[derive(Default)]
struct UsageState {
    account_id: Option<String>,
    updated_at: Option<String>,
    days: Vec<UsageDay>,
    failed: bool,
}

pub(crate) struct CodexUsage {
    path: Option<PathBuf>,
    state: Arc<Mutex<UsageState>>,
    stop: Option<mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl CodexUsage {
    pub(crate) fn start(enabled: bool) -> Result<Self> {
        // Routine service tests must never read live credentials or call ChatGPT.
        if !enabled || cfg!(test) {
            return Ok(Self::idle(None));
        }
        Self::start_with(codex_oauth::credential_path()?, PROFILE_URL.into())
    }

    fn idle(path: Option<PathBuf>) -> Self {
        Self {
            path,
            state: Arc::new(Mutex::new(UsageState::default())),
            stop: None,
            worker: None,
        }
    }

    fn start_with(path: PathBuf, url: String) -> Result<Self> {
        let mut usage = Self::idle(Some(path.clone()));
        let state = Arc::clone(&usage.state);
        let (stop, stopped) = mpsc::channel();
        let started = Instant::now();
        usage.worker = Some(thread::Builder::new().name("me-codex-usage".into()).spawn(
            move || {
                let client = Client::builder()
                    .timeout(Duration::from_secs(30))
                    .redirect(reqwest::redirect::Policy::none())
                    .retry(reqwest::retry::never())
                    .build();
                let mut first = true;
                loop {
                    refresh(&state, &path, client.as_ref().ok(), &url);
                    let delay = next_delay(first, started.elapsed());
                    first = false;
                    match stopped.recv_timeout(delay) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        _ => break,
                    }
                }
            },
        )?);
        usage.stop = Some(stop);
        Ok(usage)
    }

    pub(crate) fn snapshot(&self) -> UsageSnapshot {
        let account = self
            .path
            .as_deref()
            .and_then(|path| codex_oauth::stored_request_credential(path).ok())
            .map(|credential| credential.account_id);
        self.state
            .lock()
            .unwrap()
            .snapshot(account.as_deref(), Local::now().date_naive())
    }
}

impl Drop for CodexUsage {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn next_delay(first: bool, elapsed: Duration) -> Duration {
    if first {
        SECOND_REFRESH.saturating_sub(elapsed)
    } else {
        REFRESH_INTERVAL
    }
}

impl UsageState {
    fn snapshot(&self, account: Option<&str>, today: NaiveDate) -> UsageSnapshot {
        let range_start = (today - Days::new(29)).to_string();
        let range_end = today.to_string();
        let same_account = account.is_some() && account == self.account_id.as_deref();
        let days: Vec<_> = self
            .days
            .iter()
            .filter(|day| {
                same_account && day.start_date >= range_start && day.start_date <= range_end
            })
            .cloned()
            .collect();
        UsageSnapshot {
            status: if account.is_none() {
                "logged_out"
            } else if !same_account {
                "loading"
            } else if self.failed {
                "error"
            } else if self.updated_at.is_some() {
                "ready"
            } else {
                "loading"
            },
            updated_at: same_account.then(|| self.updated_at.clone()).flatten(),
            range_start,
            seven_day_start: (today - Days::new(6)).to_string(),
            range_end,
            total_tokens: (!days.is_empty()).then(|| days.iter().map(|day| day.tokens).sum()),
            days,
        }
    }

    fn apply(&mut self, account_id: String, result: Result<Vec<UsageDay>>) {
        if self.account_id.as_ref() != Some(&account_id) {
            *self = Self {
                account_id: Some(account_id),
                ..Self::default()
            };
        }
        match result {
            Ok(days) => {
                self.days = days;
                self.updated_at = Some(Local::now().to_rfc3339());
                self.failed = false;
            }
            Err(_) => self.failed = true,
        }
    }
}

fn refresh(state: &Mutex<UsageState>, path: &Path, client: Option<&Client>, url: &str) {
    let Ok(stored) = codex_oauth::stored_request_credential(path) else {
        *state.lock().unwrap() = UsageState::default();
        return;
    };
    let mut account_id = stored.account_id;
    let result = (|| {
        let client = client.ok_or("usage client unavailable")?;
        let mut credential = codex_oauth::request_credential(path, client, None)?;
        account_id.clone_from(&credential.account_id);
        let mut response = query(client, url, &credential)?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            // Only a rejected token gets one refresh and one retry; all other failures wait.
            credential =
                codex_oauth::request_credential(path, client, Some(&credential.access_token))?;
            account_id.clone_from(&credential.account_id);
            response = query(client, url, &credential)?;
        }
        if !response.status().is_success() {
            return Err("usage request failed".into());
        }
        let mut bytes = Vec::new();
        response
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_RESPONSE_BYTES {
            return Err("usage response too large".into());
        }
        parse_days(&bytes, Local::now().date_naive())
    })();
    // Never retain or log the profile, tokens, refresh errors, or response bodies.
    state.lock().unwrap().apply(account_id, result);
}

fn query(
    client: &Client,
    url: &str,
    credential: &codex_oauth::CodexRequestCredential,
) -> Result<reqwest::blocking::Response> {
    Ok(client
        .get(url)
        .bearer_auth(&credential.access_token)
        .header("ChatGPT-Account-Id", &credential.account_id)
        .header("User-Agent", "codex-cli")
        .send()?)
}

fn parse_days(bytes: &[u8], today: NaiveDate) -> Result<Vec<UsageDay>> {
    #[derive(Deserialize)]
    struct Profile {
        stats: Stats,
    }
    #[derive(Deserialize)]
    struct Stats {
        daily_usage_buckets: Vec<UsageDay>,
    }
    let profile: Profile = serde_json::from_slice(bytes)?;
    let mut days = BTreeMap::new();
    let mut total = 0_u64;
    for day in profile.stats.daily_usage_buckets {
        let date: NaiveDate = day.start_date.parse()?;
        if day.start_date != date.to_string() {
            return Err("usage date must be YYYY-MM-DD".into());
        }
        if date < today - Days::new(29) || date > today {
            continue;
        }
        total = total
            .checked_add(day.tokens)
            .filter(|total| *total <= MAX_SAFE_TOKENS)
            .ok_or("usage total out of range")?;
        if days.insert(date, day).is_some() {
            return Err("duplicate usage date".into());
        }
    }
    Ok(days.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };
    use tiny_http::{Response, Server, StatusCode};

    fn date(value: &str) -> NaiveDate {
        value.parse().unwrap()
    }
    fn profile(days: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&json!({"stats": {"daily_usage_buckets": days}})).unwrap()
    }
    fn credential(name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "me-usage-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("auth.json");
        write_credential(&path, "test-access", "test-account");
        path
    }
    fn write_credential(path: &Path, token: &str, account: &str) {
        fs::write(
            path,
            json!({"auth_mode":"chatgpt", "tokens": {"access_token":token,"account_id":account}})
                .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn rolling_days_include_today_preserve_gaps_and_zero_without_extrapolation() {
        let today = date("2026-03-01");
        let days = parse_days(
            &profile(json!([
                {"start_date":"2026-03-01","tokens":0},
                {"start_date":"2026-01-31","tokens":10},
                {"start_date":"2026-02-28","tokens":42},
                {"start_date":"2026-01-30","tokens":900},
                {"start_date":"2026-03-02","tokens":900}
            ])),
            today,
        )
        .unwrap();
        assert_eq!(
            days.iter().map(|day| day.tokens).collect::<Vec<_>>(),
            vec![10, 42, 0]
        );
        let mut state = UsageState::default();
        state.apply("a".into(), Ok(days));
        let snapshot = state.snapshot(Some("a"), today);
        assert_eq!(snapshot.total_tokens, Some(52));
        assert_eq!(snapshot.days.len(), 3);
        assert_eq!(snapshot.range_start, "2026-01-31");
        assert_eq!(snapshot.seven_day_start, "2026-02-23");
        assert_eq!(
            state.snapshot(Some("a"), today + Days::new(1)).total_tokens,
            Some(42)
        );
        state.apply("a".into(), Ok(vec![]));
        assert_eq!(state.snapshot(Some("a"), today).total_tokens, None);
    }

    #[test]
    fn malformed_or_ambiguous_profiles_are_not_successful_empty_data() {
        let today = date("2026-03-01");
        for bytes in [
            b"{}".to_vec(),
            b"{\"stats\":{}}".to_vec(),
            profile(json!([
                {"start_date":"2026-03-01","tokens":-1}
            ])),
            profile(json!([
                {"start_date":"2026-02-30","tokens":1}
            ])),
            profile(json!([
                {"start_date":"2026-03-01","tokens":"3"}
            ])),
            profile(json!([
                {"start_date":"2026-03-01","tokens":2},
                {"start_date":"2026-03-01","tokens":3}
            ])),
            profile(json!([
                {"start_date":"2026-03-01","tokens":MAX_SAFE_TOKENS},
                {"start_date":"2026-02-28","tokens":1}
            ])),
        ] {
            assert!(parse_days(&bytes, today).is_err());
        }
    }

    #[test]
    fn failures_keep_only_the_same_accounts_previous_success() {
        let today = date("2026-03-01");
        let mut state = UsageState::default();
        state.apply(
            "private-account".into(),
            Ok(vec![UsageDay {
                start_date: today.to_string(),
                tokens: 12,
            }]),
        );
        let updated = state.updated_at.clone();
        state.apply(
            "private-account".into(),
            Err("private-token body must not escape".into()),
        );
        let snapshot = state.snapshot(Some("private-account"), today);
        assert_eq!(snapshot.status, "error");
        assert_eq!(snapshot.total_tokens, Some(12));
        assert_eq!(snapshot.updated_at, updated);
        let serialized = serde_json::to_string(&snapshot).unwrap();
        assert!(!serialized.contains("private"));
        for account in [None, Some("other")] {
            let snapshot = state.snapshot(account, today);
            assert!(snapshot.days.is_empty());
            assert!(snapshot.updated_at.is_none());
        }
        state.apply("other".into(), Err("failed".into()));
        assert!(state.days.is_empty());
    }

    #[test]
    fn startup_has_two_attempts_then_four_hour_intervals_without_catchup_bursts() {
        assert_eq!(
            next_delay(true, Duration::from_secs(5)),
            Duration::from_secs(1795)
        );
        let mut elapsed = Duration::ZERO;
        let mut attempts = vec![elapsed.as_secs()];
        for first in [true, false, false] {
            elapsed += next_delay(first, elapsed);
            attempts.push(elapsed.as_secs());
        }
        assert_eq!(attempts, vec![0, 1800, 16200, 30600]);
        assert_eq!(
            next_delay(false, Duration::from_secs(100_000)),
            REFRESH_INTERVAL
        );
        let disabled = CodexUsage::start(false).unwrap();
        assert!(disabled.worker.is_none());
        assert_eq!(disabled.snapshot().status, "logged_out");
        assert!(CodexUsage::start(true).unwrap().worker.is_none());
    }

    #[test]
    fn one_backend_request_serves_repeated_reads_and_worker_stops_promptly() {
        let path = credential("shared");
        let server = Server::http("127.0.0.1:0").unwrap();
        let usage = CodexUsage::start_with(
            path.clone(),
            format!("http://{}/profile", server.server_addr()),
        )
        .unwrap();
        let request = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(request.url(), "/profile");
        for (key, value) in [
            ("Authorization", "Bearer test-access"),
            ("ChatGPT-Account-Id", "test-account"),
            ("User-Agent", "codex-cli"),
        ] {
            assert!(
                request
                    .headers()
                    .iter()
                    .any(|header| header.field.equiv(key) && header.value.as_str() == value)
            );
        }
        request
            .respond(Response::from_data(profile(json!([
                {"start_date":Local::now().date_naive().to_string(),"tokens":123}
            ]))))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while usage.snapshot().status != "ready" {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        for _ in 0..25 {
            assert_eq!(usage.snapshot().total_tokens, Some(123));
        }
        assert!(
            server
                .recv_timeout(Duration::from_millis(30))
                .unwrap()
                .is_none()
        );
        write_credential(&path, "other-access", "other-account");
        assert_eq!(usage.snapshot().status, "loading");
        assert!(usage.snapshot().days.is_empty());
        fs::remove_file(&path).unwrap();
        assert_eq!(usage.snapshot().status, "logged_out");
        let stopped = Instant::now();
        drop(usage);
        assert!(stopped.elapsed() < Duration::from_secs(2));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn rejected_token_reuses_rotation_once_and_rate_limits_never_retry() {
        for status in [200, 401, 429, 500] {
            let path = credential("retry");
            let server = Server::http("127.0.0.1:0").unwrap();
            let url = format!("http://{}/profile", server.server_addr());
            let state = Arc::new(Mutex::new(UsageState::default()));
            let worker_state = Arc::clone(&state);
            let worker_path = path.clone();
            let worker = thread::spawn(move || {
                let client = Client::builder()
                    .no_proxy()
                    .timeout(Duration::from_secs(3))
                    .build()
                    .unwrap();
                refresh(&worker_state, &worker_path, Some(&client), &url);
            });
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            if status == 401 {
                // Simulate another me request rotating credentials while this GET was in flight.
                write_credential(&path, "rotated-access", "test-account");
            }
            request
                .respond(
                    Response::from_data(profile(json!([]))).with_status_code(StatusCode(status)),
                )
                .unwrap();
            if status == 401 {
                let request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                assert!(
                    request
                        .headers()
                        .iter()
                        .any(|header| header.field.equiv("Authorization")
                            && header.value.as_str() == "Bearer rotated-access")
                );
                request.respond(Response::empty(StatusCode(401))).unwrap();
            }
            worker.join().unwrap();
            assert_eq!(state.lock().unwrap().failed, status != 200);
            assert!(
                server
                    .recv_timeout(Duration::from_millis(20))
                    .unwrap()
                    .is_none()
            );
            fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }
}
