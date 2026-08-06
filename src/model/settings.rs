//! Three buckets for state, distinguished by one question each.
//!
//! *Can it change after launch?* No → [`Config`]: the server URL, the vault
//! path, which capabilities exist at all. *Is it the same for every thread, and
//! does it survive a restart?* Yes → [`Settings`]: sampling, thinking budget
//! and visibility, compaction, the window's shape. *Does it belong to one
//! conversation?* Yes → `thread::Thread`, which is not in this file.
//!
//! Precedence on load is **CLI flag > saved file > default**, and loading never
//! writes: a one-off `--temp 0.2` wins for that run without quietly becoming
//! the new normal. Only the preferences dialog writes.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

/// llama-server's default address.
pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:8080";

/// What happened when a file was read.
///
/// Returned alongside the value rather than logged, so the UI decides whether
/// anything is worth saying — which for a missing file on first run, it is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadOutcome {
    Loaded,
    /// No file yet. The normal first run.
    Fresh,
    /// Unreadable, so it was set aside and started over. Nothing is lost but
    /// preferences, which are a dialog away.
    Recovered {
        backup: PathBuf,
    },
}

/// Bootstrap: settled at launch and immutable for the session.
/// Where somebody's mail lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailAccount {
    pub host: String,
    #[serde(default = "default_imap_port")]
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub password: String,
    /// Off only for a bridge on localhost, which is why it defaults to on.
    #[serde(default = "yes")]
    pub tls: bool,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub smtp_host: String,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
}

fn default_imap_port() -> u16 {
    993
}

fn default_smtp_port() -> u16 {
    465
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,
    /// Where llama-server is.
    #[serde(default = "default_server_url")]
    pub server_url: String,
    /// Brain's vault, which is where memory lives. `None` until it is chosen,
    /// and the memory tools are simply absent until then.
    #[serde(default)]
    pub vault: Option<PathBuf>,
    /// Named only when something in front of llama-server routes by model.
    #[serde(default)]
    pub model: Option<String>,
    /// Master switches. A capability switched off here is not offered to the
    /// model at all, in any context.
    #[serde(default = "yes")]
    pub memory: bool,
    #[serde(default = "yes")]
    pub web: bool,
    /// Where the weather is, as a coordinate.
    ///
    /// A coordinate rather than a postcode because that is what the API takes
    /// and because it is the one form that is never ambiguous — a postcode
    /// covers several square miles and the services that resolve one disagree
    /// by a few kilometres, which is enough to land on a different forecast
    /// grid cell. Absent until it is set, and the tool says so rather than
    /// guessing at a location.
    #[serde(default)]
    pub weather_latitude: Option<f64>,
    #[serde(default)]
    pub weather_longitude: Option<f64>,
    /// The Exa key, if it is kept here rather than in the environment.
    ///
    /// This is a secret in a plain file, which is what `~/.config` is for on a
    /// single-user machine — the same posture as an SSH config. `EXA_API_KEY`
    /// takes precedence for anyone who would rather it were not written down.
    #[serde(default)]
    pub exa_api_key: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            server_url: DEFAULT_SERVER_URL.to_string(),
            vault: None,
            model: None,
            memory: true,
            web: true,
            weather_latitude: None,
            weather_longitude: None,
            exa_api_key: None,
        }
    }
}

impl Config {
    /// The configured weather location, if it is one.
    ///
    /// Both halves or neither: a latitude with no longitude is a half-filled
    /// form, not a place, and treating the missing half as zero would ask for
    /// the weather in the Gulf of Guinea.
    pub fn weather_point(&self) -> Option<crate::model::weather::Point> {
        let point = crate::model::weather::Point {
            latitude: self.weather_latitude?,
            longitude: self.weather_longitude?,
        };
        point.is_plausible().then_some(point)
    }
}

/// What the command line asked for, over and above the file. Every field is
/// optional because absent means "whatever was saved".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overrides {
    pub server_url: Option<String>,
    pub vault: Option<PathBuf>,
    pub model: Option<String>,
    pub memory: Option<bool>,
    pub web: Option<bool>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_tokens: Option<u32>,
    pub reasoning_budget: Option<i32>,
    pub compaction: Option<bool>,
}

impl Config {
    /// `$XDG_CONFIG_HOME/familiar/config.json`, falling back to `~/.config`.
    pub fn default_path() -> PathBuf {
        config_directory().join("config.json")
    }

    pub fn load(path: &Path) -> (Self, LoadOutcome) {
        load_or_recover(path)
    }

    pub fn save(&self, path: &Path) -> Result<(), SettingsError> {
        write_json(path, self)
    }

    /// CLI over file. Returns a new value rather than mutating, so the saved
    /// config and the running one are visibly different things.
    pub fn with(mut self, overrides: &Overrides) -> Self {
        if let Some(url) = &overrides.server_url {
            self.server_url = url.clone();
        }
        if let Some(vault) = &overrides.vault {
            self.vault = Some(vault.clone());
        }
        if let Some(model) = &overrides.model {
            self.model = Some(model.clone());
        }
        if let Some(memory) = overrides.memory {
            self.memory = memory;
        }
        if let Some(web) = overrides.web {
            self.web = web;
        }
        self
    }
}

/// Persisted preferences: the same for every thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_top_p")]
    pub top_p: f32,
    /// Honoured only when llama-server was started without
    /// `--reasoning-budget`; the preferences dialog says so.
    #[serde(default)]
    pub reasoning_budget: Option<i32>,
    #[serde(default = "yes")]
    pub show_thinking: bool,
    /// Send the model's own prior reasoning back to it.
    ///
    /// Off for a server or template that cannot use it. On needs llama-server
    /// started with `preserve_thinking` in its `--chat-template-kwargs`, which
    /// the froggeric template honours by re-emitting the text inside `<think>`
    /// tags. Measured to roughly halve how much the model re-derives per turn
    /// while keeping the cached prefix intact — see `Thread::messages_with_reasoning`.
    #[serde(default = "yes")]
    pub carry_reasoning: bool,
    /// How long the answer may be, in tokens. `None` lets the server decide,
    /// which for llama-server means until it stops or the context runs out.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default = "yes")]
    pub compaction: bool,
    /// Read every finished turn for durable facts and save them without being
    /// asked.
    ///
    /// On by default, because the alternative is an assistant that only
    /// remembers what you thought to tell it to remember. Off is a real
    /// preference and not only a debugging switch: some people want their notes
    /// to contain exactly what they put there.
    #[serde(default = "yes")]
    pub passive_memory: bool,
    /// Consolidate what has been saved, on a schedule, overnight.
    #[serde(default = "yes")]
    pub dreaming: bool,
    /// The hour it happens, local. Three in the morning: late enough that
    /// nobody is mid-conversation, early enough that the machine is usually
    /// still awake.
    #[serde(default = "default_dream_hour")]
    pub dream_hour: u32,
    /// When it last ran, so a laptop that was asleep at three does not wake up
    /// and immediately consolidate at lunchtime.
    #[serde(default)]
    pub last_dream: Option<chrono::DateTime<chrono::Utc>>,
    /// Which CLI `escalate` asks: `claude` or `codex`. Both are already
    /// installed and signed in on a machine that has them; neither is reached
    /// without a per-call approval.
    #[serde(default = "default_escalate_to")]
    pub escalate_to: String,
    /// A specific model for it, if the default of whichever CLI is not wanted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalate_model: Option<String>,
    /// Look over the day now and then and say something only if it is worth
    /// saying. Off by default: an assistant that speaks up uninvited has to
    /// earn the right, and the way it earns it is by being switched on.
    #[serde(default)]
    pub lookout: bool,
    /// Keep running with the window closed, so a schedule fires whether or not
    /// anybody has the app open.
    ///
    /// Off by default: an app that will not quit when you close it is a
    /// surprise, and it has to be asked for. On, closing the window leaves the
    /// process running and the jobs keep their clock.
    #[serde(default)]
    pub background: bool,
    /// How often, in hours.
    #[serde(default = "default_lookout_hours")]
    pub lookout_hours: u32,
    /// When it last looked, so a laptop that was asleep does not wake up and
    /// immediately check — the same rule the nightly pass keeps.
    #[serde(default)]
    pub last_lookout: Option<chrono::DateTime<chrono::Utc>>,
    /// The mail account, when there is one. The password sits beside the Exa
    /// key in this file, which is the same trade this application already
    /// made — a keyring would be better and is a change to make once, for
    /// both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mail: Option<MailAccount>,
    /// Turns never folded into the rolling summary.
    #[serde(default = "default_keep_recent")]
    pub keep_recent_turns: usize,
    /// What the context-usage bar measures against, until the server's own
    /// `/props` says otherwise.
    #[serde(default = "default_context_window")]
    pub context_window: u32,
    /// The keyboard shortcut that starts listening, as GNOME spells it.
    ///
    /// `None` means voice has never been switched on. It is off until asked
    /// for, because registering a system-wide key on somebody's behalf is not a
    /// thing to do at first launch — and because the microphone is involved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_shortcut: Option<String>,
    /// Which input `pw-record` is told to take. Empty is the system default,
    /// which is what almost everybody wants.
    #[serde(default)]
    pub voice_source: String,
    /// How the answer is read back: `off`, `desktop` or `endpoint`.
    #[serde(default = "default_voice_reply")]
    pub voice_reply: String,
    /// An OpenAI-shaped speech server, when `voice_reply` is `endpoint`.
    #[serde(default)]
    pub voice_endpoint: String,
    /// The voice's name, as that server spells it.
    #[serde(default = "default_voice_name")]
    pub voice_name: String,
    /// What sample rate its raw PCM comes back at. Kokoro's is 24 kHz.
    #[serde(default = "default_voice_rate")]
    pub voice_rate: u32,
    /// How long after a spoken exchange the next one carries on the same chat.
    /// Zero starts a new chat every time.
    #[serde(default = "default_follow_up")]
    pub voice_follow_up: i64,
    /// Listen again as soon as it has finished answering, so a conversation is
    /// a conversation rather than a series of dictations. On: saying nothing
    /// ends it after a few seconds, so it costs a pause to leave and a key
    /// press to resume either way.
    #[serde(default = "yes")]
    pub voice_converse: bool,
    #[serde(default)]
    pub window_width: Option<i32>,
    #[serde(default)]
    pub window_height: Option<i32>,
    #[serde(default)]
    pub window_maximized: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            temperature: default_temperature(),
            top_p: default_top_p(),
            max_tokens: None,
            reasoning_budget: None,
            show_thinking: true,
            carry_reasoning: true,
            compaction: true,
            passive_memory: true,
            dreaming: true,
            dream_hour: default_dream_hour(),
            last_dream: None,
            lookout: false,
            background: false,
            lookout_hours: default_lookout_hours(),
            last_lookout: None,
            mail: None,
            escalate_to: default_escalate_to(),
            escalate_model: None,
            keep_recent_turns: default_keep_recent(),
            context_window: default_context_window(),
            voice_shortcut: None,
            voice_source: String::new(),
            voice_reply: default_voice_reply(),
            voice_endpoint: String::new(),
            voice_name: default_voice_name(),
            voice_rate: default_voice_rate(),
            voice_follow_up: default_follow_up(),
            voice_converse: true,
            window_width: None,
            window_height: None,
            window_maximized: false,
        }
    }
}

impl Settings {
    /// When consolidation should next happen, as the heartbeat's own schedule
    /// model expresses it.
    ///
    /// Reusing [`crate::model::heartbeat::Schedule`] rather than inventing a
    /// second one: the hard part of a schedule is not "daily at three", it is
    /// deciding whether to fire after the machine was asleep through it, and
    /// that arithmetic is already written and already tested.
    pub fn dream_schedule(&self) -> crate::model::heartbeat::Schedule {
        crate::model::heartbeat::Schedule::Daily {
            at: chrono::NaiveTime::from_hms_opt(self.dream_hour.min(23), 0, 0).unwrap_or_default(),
        }
    }

    /// How often the proactive check runs, in the same terms as everything else
    /// that runs on its own.
    ///
    /// It had its own arithmetic — a raw hours comparison against `last_lookout`
    /// — which was a third notion of "due" beside the schedule model and the
    /// jobs list. Same reasoning as [`Self::dream_schedule`]: the hard part is
    /// not "every four hours", it is deciding whether to fire after the machine
    /// was asleep through it, and that is written and tested once.
    pub fn lookout_schedule(&self) -> crate::model::heartbeat::Schedule {
        crate::model::heartbeat::Schedule::Hours {
            hours: self.lookout_hours.max(1),
        }
    }

    /// `$XDG_CONFIG_HOME/familiar/settings.json`.
    pub fn default_path() -> PathBuf {
        config_directory().join("settings.json")
    }

    pub fn load(path: &Path) -> (Self, LoadOutcome) {
        load_or_recover(path)
    }

    pub fn save(&self, path: &Path) -> Result<(), SettingsError> {
        write_json(path, self)
    }

    pub fn with(mut self, overrides: &Overrides) -> Self {
        if let Some(temperature) = overrides.temperature {
            self.temperature = temperature;
        }
        if let Some(top_p) = overrides.top_p {
            self.top_p = top_p;
        }
        if let Some(max_tokens) = overrides.max_tokens {
            self.max_tokens = Some(max_tokens);
        }
        if let Some(budget) = overrides.reasoning_budget {
            self.reasoning_budget = Some(budget);
        }
        if let Some(compaction) = overrides.compaction {
            self.compaction = compaction;
        }
        self
    }
}

fn default_version() -> u32 {
    SCHEMA_VERSION
}

fn default_server_url() -> String {
    DEFAULT_SERVER_URL.to_string()
}

fn default_temperature() -> f32 {
    0.7
}

fn default_top_p() -> f32 {
    0.95
}

fn default_lookout_hours() -> u32 {
    4
}

fn default_escalate_to() -> String {
    crate::model::escalate::Backend::default()
        .label()
        .to_string()
}

fn default_keep_recent() -> usize {
    6
}

fn default_dream_hour() -> u32 {
    3
}

fn default_context_window() -> u32 {
    32_768
}

/// The desktop's own synthesiser, because it is already installed. A better
/// voice is a server away and a preference away; silence would make the
/// feature half a feature by default.
fn default_voice_reply() -> String {
    "desktop".to_string()
}

/// Kokoro's default American voice. Meaningless to speech-dispatcher, which is
/// why it only reads it under `endpoint`.
fn default_voice_name() -> String {
    "af_heart".to_string()
}

fn default_voice_rate() -> u32 {
    24_000
}

/// Long enough to ask a follow-up after reading the answer, short enough that
/// the next unrelated thing does not land in the same chat.
fn default_follow_up() -> i64 {
    8
}

fn yes() -> bool {
    true
}

fn config_directory() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("familiar")
}

fn load_or_recover<T>(path: &Path) -> (T, LoadOutcome)
where
    T: Default + serde::de::DeserializeOwned,
{
    let Ok(text) = fs::read_to_string(path) else {
        return (T::default(), LoadOutcome::Fresh);
    };
    match serde_json::from_str::<T>(&text) {
        Ok(value) => (value, LoadOutcome::Loaded),
        Err(_) => {
            // Keep the unreadable file rather than deleting it: it is the only
            // copy of whatever was in there.
            let backup = path.with_extension("json.corrupt");
            let _ = fs::rename(path, &backup);
            (T::default(), LoadOutcome::Recovered { backup })
        }
    }
}

/// Write atomically: tmp, flush, fsync, rename.
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), SettingsError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| SettingsError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let text = serde_json::to_string_pretty(value).map_err(SettingsError::Serialize)?;

    let temporary = path.with_extension("json.tmp");
    let io = |source| SettingsError::Io {
        path: temporary.clone(),
        source,
    };
    let mut file = fs::File::create(&temporary).map_err(io)?;
    file.write_all(text.as_bytes()).map_err(io)?;
    file.flush().map_err(io)?;
    file.sync_all().map_err(io)?;
    drop(file);

    fs::rename(&temporary, path).map_err(|source| SettingsError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[derive(Debug)]
pub enum SettingsError {
    Io { path: PathBuf, source: io::Error },
    Serialize(serde_json::Error),
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Serialize(source) => write!(f, "{source}"),
        }
    }
}

impl std::error::Error for SettingsError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_a_first_run_not_an_error() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (config, outcome) = Config::load(&directory.path().join("config.json"));
        assert_eq!(outcome, LoadOutcome::Fresh);
        assert_eq!(config.server_url, DEFAULT_SERVER_URL);
        assert_eq!(config.vault, None);
    }

    #[test]
    fn settings_round_trip() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("familiar/settings.json");
        let settings = Settings {
            temperature: 0.2,
            show_thinking: false,
            window_width: Some(1100),
            ..Default::default()
        };
        settings.save(&path).expect("save");

        let (read, outcome) = Settings::load(&path);
        assert_eq!(outcome, LoadOutcome::Loaded);
        assert_eq!(read, settings);
    }

    #[test]
    fn an_unreadable_file_is_set_aside_not_deleted() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("settings.json");
        fs::write(&path, "{not json").expect("write");

        let (settings, outcome) = Settings::load(&path);
        assert_eq!(settings, Settings::default());
        match outcome {
            LoadOutcome::Recovered { backup } => {
                assert_eq!(fs::read_to_string(backup).expect("backup"), "{not json");
            }
            other => panic!("expected recovery, got {other:?}"),
        }
    }

    #[test]
    fn a_file_written_by_an_earlier_build_still_loads() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("settings.json");
        fs::write(&path, r#"{"temperature":0.3}"#).expect("write");

        let (settings, outcome) = Settings::load(&path);
        assert_eq!(outcome, LoadOutcome::Loaded);
        assert_eq!(settings.temperature, 0.3);
        // Absent fields are defaults, not a corrupt file.
        assert_eq!(settings.top_p, default_top_p());
        assert!(settings.show_thinking);
    }

    #[test]
    fn a_night_the_machine_slept_through_is_skipped_rather_than_done_at_lunchtime() {
        // The same rule a scheduled thread follows, for the same reason: a
        // consolidation pass at 14:00 competes with the person using the
        // application, and nothing about it is urgent.
        use chrono::TimeZone;
        let settings = Settings::default();
        let local = |text: &str| {
            chrono::Local
                .from_local_datetime(
                    &chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").expect("a time"),
                )
                .earliest()
                .expect("a local time")
        };
        let schedule = settings.dream_schedule();
        let last = local("2026-08-01 03:00");
        let on_time = crate::model::heartbeat::Recovery::OnTime;
        assert!(schedule
            .due(Some(last), local("2026-08-02 03:00"), on_time)
            .is_some());
        assert!(schedule
            .due(Some(last), local("2026-08-02 14:00"), on_time)
            .is_none());
    }

    #[test]
    fn the_lookout_uses_the_same_arithmetic_as_everything_else() {
        // It had its own hours comparison, which is the third notion of "due"
        // this replaced — and the one that had never been asked what to do
        // about a machine that was asleep through an occurrence.
        use chrono::TimeZone;
        let local = |text: &str| {
            chrono::Local
                .from_local_datetime(
                    &chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").expect("a time"),
                )
                .earliest()
                .expect("a local time")
        };
        let settings = Settings {
            lookout_hours: 4,
            ..Settings::default()
        };
        let schedule = settings.lookout_schedule();
        let last = local("2026-08-03 08:00");
        let whenever = crate::model::heartbeat::Recovery::Whenever;

        assert!(
            schedule
                .due(Some(last), local("2026-08-03 11:00"), whenever)
                .is_none(),
            "three hours in is not yet four"
        );
        assert!(schedule
            .due(Some(last), local("2026-08-03 12:00"), whenever)
            .is_some());
        // Asleep for two days: one check, not twelve.
        assert!(schedule
            .due(Some(last), local("2026-08-05 12:00"), whenever)
            .is_some());
    }

    #[test]
    fn a_nonsense_lookout_interval_cannot_make_it_run_every_tick() {
        let settings = Settings {
            lookout_hours: 0,
            ..Settings::default()
        };
        assert_eq!(
            settings.lookout_schedule(),
            crate::model::heartbeat::Schedule::Hours { hours: 1 }
        );
    }

    #[test]
    fn remembering_and_consolidating_are_both_on_out_of_the_box() {
        // An assistant that only remembers what you thought to tell it to
        // remember is one you have to manage.
        let settings = Settings::default();
        assert!(settings.passive_memory);
        assert!(settings.dreaming);
        assert_eq!(settings.dream_hour, 3);
    }

    #[test]
    fn reasoning_is_carried_by_default() {
        // The template this is used with sets preserve_thinking, so the
        // default is to give it something to preserve.
        assert!(Settings::default().carry_reasoning);
    }

    #[test]
    fn a_flag_beats_the_file_for_one_run_only() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("settings.json");
        Settings {
            temperature: 0.7,
            ..Default::default()
        }
        .save(&path)
        .expect("save");

        let overrides = Overrides {
            temperature: Some(0.2),
            ..Default::default()
        };
        let (saved, _) = Settings::load(&path);
        let running = saved.with(&overrides);

        assert_eq!(running.temperature, 0.2);
        // The file is untouched: loading never writes.
        let (again, _) = Settings::load(&path);
        assert_eq!(again.temperature, 0.7);
    }

    #[test]
    fn an_absent_override_leaves_the_saved_value_alone() {
        let settings = Settings {
            temperature: 0.4,
            compaction: false,
            ..Default::default()
        }
        .with(&Overrides::default());
        assert_eq!(settings.temperature, 0.4);
        assert!(!settings.compaction);
    }

    #[test]
    fn a_capability_switched_off_at_launch_stays_off() {
        let config = Config::default().with(&Overrides {
            web: Some(false),
            ..Default::default()
        });
        assert!(!config.web);
        assert!(config.memory);
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let directory = tempfile::tempdir().expect("temp dir");
        Settings::default()
            .save(&directory.path().join("settings.json"))
            .expect("save");

        let leftovers: Vec<_> = fs::read_dir(directory.path())
            .expect("read dir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn config_and_settings_are_separate_files_under_one_directory() {
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg");
        assert_eq!(
            Config::default_path(),
            PathBuf::from("/tmp/xdg/familiar/config.json")
        );
        assert_eq!(
            Settings::default_path(),
            PathBuf::from("/tmp/xdg/familiar/settings.json")
        );
        match previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }
}
