//! Preferences.
//!
//! `AdwPreferencesDialog` — pages, groups and search for free, presented
//! adaptively. What is in here is the [`Settings`] bucket plus the two
//! [`Config`] values that decide where things are: the server and the vault.
//!
//! Changing the server URL or the vault takes effect at the next launch, and
//! the rows say so rather than appearing to work and not.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib::{self, clone};

use crate::model::settings::{Config, Settings};

/// Everything the dialog can change, handed back whenever it changes.
#[derive(Debug, Clone, PartialEq)]
pub struct Preferences {
    pub config: Config,
    pub settings: Settings,
}

/// Gmail's servers, which is what "an email account" means for most people and
/// certainly for this one. Filled in by the preset so nobody has to know them.
const GMAIL: (&str, u16, &str, u16) = ("imap.gmail.com", 993, "smtp.gmail.com", 465);

/// The mail account, in the two rows that are usually all anybody needs.
///
/// **Why an app password and not OAuth.** Google stopped accepting an account
/// password over IMAP years ago, so the choice is between an app password and a
/// full OAuth 2.0 flow. OAuth for a desktop application means registering a
/// Google Cloud project, shipping a client secret inside an application anyone
/// can read — where it is not a secret — running a loopback HTTP server to
/// catch the redirect, and storing a refresh token. An app password is sixteen
/// characters the user pastes once, is revocable on its own without touching
/// the account password, and is scoped to mail. For a single-user desktop
/// application talking to one mailbox, it is the smaller and more honest of the
/// two.
///
/// The password sits in the settings file beside the Exa key, which is the same
/// trade this application already made. A keyring would be better and is one
/// change to make once, for both.
pub fn mail_group(
    state: &Rc<RefCell<Preferences>>,
    changed: &Rc<impl Fn() + 'static>,
    current: &Preferences,
) -> adw::PreferencesGroup {
    use crate::model::settings::MailAccount;

    let account = current.settings.mail.clone().unwrap_or(MailAccount {
        host: GMAIL.0.into(),
        port: GMAIL.1,
        user: String::new(),
        password: String::new(),
        tls: true,
        from: String::new(),
        smtp_host: GMAIL.2.into(),
        smtp_port: GMAIL.3,
    });

    let group = adw::PreferencesGroup::builder()
        .title("Mail")
        .description(
            "Gmail needs an app password, not the one you sign in with — turn on 2-Step \
             Verification, then make one at myaccount.google.com/apppasswords. Sending and \
             deleting always ask you first.",
        )
        .build();

    let address = adw::EntryRow::builder().title("Email Address").build();
    address.set_text(&account.user);

    let password = adw::PasswordEntryRow::builder()
        .title("App Password")
        .build();
    password.set_text(&account.password);

    // The two that only a non-Gmail account needs, folded away. An expander
    // rather than a second page: somebody on Fastmail has to be able to get at
    // them, and everybody else should never see them.
    let server = adw::ExpanderRow::builder()
        .title("Server")
        .subtitle(format!("{}, and {}", account.host, account.smtp_host))
        .build();
    let host = adw::EntryRow::builder().title("IMAP Server").build();
    host.set_text(&account.host);
    let smtp = adw::EntryRow::builder().title("SMTP Server").build();
    smtp.set_text(&account.smtp_host);
    let imap_port = adw::SpinRow::with_range(1.0, 65535.0, 1.0);
    imap_port.set_title("IMAP Port");
    imap_port.set_value(f64::from(account.port));
    let smtp_port = adw::SpinRow::with_range(1.0, 65535.0, 1.0);
    smtp_port.set_title("SMTP Port");
    smtp_port.set_value(f64::from(account.smtp_port));
    let tls = adw::SwitchRow::builder()
        .title("TLS")
        .subtitle("Off only for a bridge on this machine, which has no network to protect")
        .active(account.tls)
        .build();
    for row in [&host, &smtp] {
        server.add_row(row);
    }
    server.add_row(&imap_port);
    server.add_row(&smtp_port);
    server.add_row(&tls);

    // One reader for every row, so a half-filled form is a half-filled account
    // rather than five separate writes racing each other.
    let read = clone!(
        #[strong]
        state,
        #[strong]
        changed,
        #[weak]
        address,
        #[weak]
        password,
        #[weak]
        host,
        #[weak]
        smtp,
        #[weak]
        imap_port,
        #[weak]
        smtp_port,
        #[weak]
        tls,
        #[weak]
        server,
        move || {
            let user = address.text().trim().to_string();
            let host_name = host.text().trim().to_string();
            let smtp_name = smtp.text().trim().to_string();
            server.set_subtitle(&format!("{host_name}, and {smtp_name}"));
            {
                let mut state = state.borrow_mut();
                // An address with nothing in it is not an account. Clearing the
                // field is how somebody removes one, and leaving a husk behind
                // would keep Mail switched on with nothing to reach.
                state.settings.mail = (!user.is_empty()).then(|| MailAccount {
                    host: host_name,
                    port: imap_port.value() as u16,
                    user: user.clone(),
                    password: password.text().to_string(),
                    tls: tls.is_active(),
                    // The address is who the mail is from, unless somebody has a
                    // reason for it not to be.
                    from: user,
                    smtp_host: smtp_name,
                    smtp_port: smtp_port.value() as u16,
                });
            }
            changed();
        }
    );

    for row in [&address, &host, &smtp] {
        row.connect_changed(clone!(
            #[strong]
            read,
            move |_| read()
        ));
    }
    password.connect_changed(clone!(
        #[strong]
        read,
        move |_| read()
    ));
    for row in [&imap_port, &smtp_port] {
        row.connect_changed(clone!(
            #[strong]
            read,
            move |_| read()
        ));
    }
    tls.connect_active_notify(clone!(
        #[strong]
        read,
        move |_| read()
    ));

    group.add(&address);
    group.add(&password);
    group.add(&server);
    group
}

/// Show the dialog. `on_change` is called on every edit — there is no Save
/// button, which is the GNOME pattern: a preference takes effect when you set
/// it.
pub fn present<F>(parent: &impl IsA<gtk::Widget>, current: &Preferences, on_change: F)
where
    F: Fn(Preferences) + 'static,
{
    let state = Rc::new(RefCell::new(current.clone()));
    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Preferences");

    let on_change = Rc::new(on_change);
    let changed = {
        let state = state.clone();
        let on_change = on_change.clone();
        move || on_change(state.borrow().clone())
    };
    let changed = Rc::new(changed);

    // -- the model -----------------------------------------------------------
    let page = adw::PreferencesPage::builder()
        .title("Model")
        .icon_name("preferences-system-symbolic")
        .build();

    let server = adw::EntryRow::builder().title("Server").build();
    server.set_text(&current.config.server_url);
    server.connect_changed(clone!(
        #[strong]
        state,
        #[strong]
        changed,
        move |row| {
            state.borrow_mut().config.server_url = row.text().to_string();
            changed();
        }
    ));

    let connection = adw::PreferencesGroup::builder()
        .title("Connection")
        .description("Where llama-server is. Takes effect at the next launch.")
        .build();
    connection.add(&server);
    page.add(&connection);

    let temperature = adw::SpinRow::with_range(0.0, 2.0, 0.05);
    temperature.set_title("Temperature");
    temperature.set_subtitle("Higher is more varied");
    temperature.set_digits(2);
    temperature.set_value(f64::from(current.settings.temperature));
    temperature.connect_changed(clone!(
        #[strong]
        state,
        #[strong]
        changed,
        move |row| {
            state.borrow_mut().settings.temperature = row.value() as f32;
            changed();
        }
    ));

    let top_p = adw::SpinRow::with_range(0.0, 1.0, 0.05);
    top_p.set_title("Top-p");
    top_p.set_digits(2);
    top_p.set_value(f64::from(current.settings.top_p));
    top_p.connect_changed(clone!(
        #[strong]
        state,
        #[strong]
        changed,
        move |row| {
            state.borrow_mut().settings.top_p = row.value() as f32;
            changed();
        }
    ));

    let sampling = adw::PreferencesGroup::builder()
        .title("Sampling")
        .description("Applied to the next turn. Changing these mid-conversation does not disturb the cached prompt.")
        .build();
    sampling.add(&temperature);
    sampling.add(&top_p);
    page.add(&sampling);
    dialog.add(&page);

    // -- thinking and memory -------------------------------------------------
    let behaviour = adw::PreferencesPage::builder()
        .title("Assistant")
        .icon_name("emblem-system-symbolic")
        .build();

    let thinking = adw::SwitchRow::builder()
        .title("Show Thinking")
        .subtitle("Keep the model's reasoning behind a disclosure on each turn")
        .active(current.settings.show_thinking)
        .build();
    thinking.connect_active_notify(clone!(
        #[strong]
        state,
        #[strong]
        changed,
        move |row| {
            state.borrow_mut().settings.show_thinking = row.is_active();
            changed();
        }
    ));

    let carry = adw::SwitchRow::builder()
        .title("Send Reasoning Back")
        .subtitle("Let the model see its own earlier thinking. Needs a template that preserves it")
        .active(current.settings.carry_reasoning)
        .build();
    carry.connect_active_notify(clone!(
        #[strong]
        state,
        #[strong]
        changed,
        move |row| {
            state.borrow_mut().settings.carry_reasoning = row.is_active();
            changed();
        }
    ));

    let budget = adw::SpinRow::with_range(0.0, 32768.0, 256.0);
    budget.set_title("Thinking Budget");
    budget.set_subtitle("Tokens the model may spend thinking. 0 for no limit");
    budget.set_value(f64::from(
        current.settings.reasoning_budget.unwrap_or(0).max(0),
    ));
    budget.connect_changed(clone!(
        #[strong]
        state,
        #[strong]
        changed,
        move |row| {
            let value = row.value() as i32;
            state.borrow_mut().settings.reasoning_budget = (value > 0).then_some(value);
            changed();
        }
    ));

    let length = adw::SpinRow::with_range(0.0, 32768.0, 256.0);
    length.set_title("Answer Limit");
    length.set_subtitle("Tokens an answer may run to, thinking included. 0 for no limit");
    length.set_value(f64::from(current.settings.max_tokens.unwrap_or(0)));
    length.connect_changed(clone!(
        #[strong]
        state,
        #[strong]
        changed,
        move |row| {
            let value = row.value() as u32;
            state.borrow_mut().settings.max_tokens = (value > 0).then_some(value);
            changed();
        }
    ));

    let reasoning = adw::PreferencesGroup::builder()
        .title("Reasoning")
        .description("The thinking budget is honoured only when llama-server was started without --reasoning-budget. Sending reasoning back needs preserve_thinking in its --chat-template-kwargs.")
        .build();
    reasoning.add(&thinking);
    reasoning.add(&carry);
    reasoning.add(&budget);
    reasoning.add(&length);
    behaviour.add(&reasoning);

    let compaction = adw::SwitchRow::builder()
        .title("Summarize Long Chats")
        .subtitle("Fold older turns into a summary so a long conversation keeps fitting")
        .active(current.settings.compaction)
        .build();

    // "Turns Kept in Full" on its own read as the trigger — fold once a chat
    // passes this many turns — which is not what the code does and has not been
    // since compaction was rewritten to measure tokens. Nothing folds until a
    // chat fills `compaction::FOLD_ABOVE` of the window the server reported,
    // and *then* this decides how much of the tail survives. A forty-turn chat
    // of short exchanges is never folded at all, and someone who had set this
    // to 6 would have had no way to know that from the dialog.
    let keep = adw::SpinRow::with_range(2.0, 40.0, 1.0);
    keep.set_title("Recent Turns Kept Whole");
    keep.set_subtitle(&format!(
        "How much of the end of the chat is left untouched when a fold happens. Nothing is \
         folded until the conversation fills {:.0}% of the context window",
        crate::model::compaction::FOLD_ABOVE * 100.0
    ));
    keep.set_value(current.settings.keep_recent_turns as f64);
    keep.set_sensitive(current.settings.compaction);

    compaction.connect_active_notify(clone!(
        #[strong]
        state,
        #[strong]
        changed,
        #[weak]
        keep,
        move |row| {
            state.borrow_mut().settings.compaction = row.is_active();
            // Off means off: the row that only matters when it is on goes with
            // it rather than sitting there doing nothing.
            keep.set_sensitive(row.is_active());
            changed();
        }
    ));
    keep.connect_changed(clone!(
        #[strong]
        state,
        #[strong]
        changed,
        move |row| {
            state.borrow_mut().settings.keep_recent_turns = row.value() as usize;
            changed();
        }
    ));

    let length = adw::PreferencesGroup::builder()
        .title("Long Conversations")
        .description(
            "Measured against the context window the server reports, not counted in turns — a \
             long chat of short exchanges is never folded. Nothing is ever removed from the \
             transcript, only from what the model is shown, and the chat says when it \
             happened.",
        )
        .build();
    length.add(&compaction);
    length.add(&keep);
    behaviour.add(&length);

    // -- the vault -----------------------------------------------------------
    let vault = adw::ActionRow::builder()
        .title("Notes")
        .subtitle(
            current
                .config
                .vault
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "Brain's vault, when it has one".into()),
        )
        .build();

    let choose = gtk::Button::with_label("Choose…");
    choose.set_valign(gtk::Align::Center);
    choose.connect_clicked(clone!(
        #[strong]
        state,
        #[strong]
        changed,
        #[weak]
        vault,
        #[weak]
        dialog,
        move |_| {
            let chooser = gtk::FileDialog::builder()
                .title("Choose Your Notes")
                .build();
            chooser.select_folder(
                dialog.root().and_downcast_ref::<gtk::Window>(),
                gtk::gio::Cancellable::NONE,
                clone!(
                    #[strong]
                    state,
                    #[strong]
                    changed,
                    #[weak]
                    vault,
                    move |result| {
                        let Ok(folder) = result else { return };
                        let Some(path) = folder.path() else { return };
                        vault.set_subtitle(&path.display().to_string());
                        state.borrow_mut().config.vault = Some(path);
                        changed();
                    }
                ),
            );
        }
    ));
    vault.add_suffix(&choose);

    let exa = adw::PasswordEntryRow::builder()
        .title("Exa API Key")
        .build();
    exa.set_text(current.config.exa_api_key.as_deref().unwrap_or(""));
    exa.connect_changed(clone!(
        #[strong]
        state,
        #[strong]
        changed,
        move |row| {
            let key = row.text().trim().to_string();
            state.borrow_mut().config.exa_api_key = (!key.is_empty()).then_some(key);
            changed();
        }
    ));

    let web = adw::PreferencesGroup::builder()
        .title("Web")
        .description("Searching uses Exa, from dashboard.exa.ai/api-keys. Set EXA_API_KEY in the environment instead to keep it out of the config file. A search is the only thing here that leaves this machine, and only when a project has the web switched on.")
        .build();
    web.add(&exa);
    behaviour.add(&web);

    // A coordinate, not a postcode. That is what the API takes, and a postcode
    // covers several square miles — the services that resolve one disagree by
    // a few kilometres, which is enough to land on a different forecast grid.
    let coordinate = |title: &str, value: Option<f64>| {
        let row = adw::EntryRow::builder().title(title).build();
        row.set_text(&value.map(|v| format!("{v}")).unwrap_or_default());
        row
    };
    let latitude = coordinate("Latitude", current.config.weather_latitude);
    let longitude = coordinate("Longitude", current.config.weather_longitude);

    // Parsed on every keystroke, and a half-typed "43." is simply not a number
    // yet — the field goes empty rather than shouting at someone mid-entry.
    let read = clone!(
        #[strong]
        state,
        #[strong]
        changed,
        #[weak]
        latitude,
        #[weak]
        longitude,
        move || {
            let number = |row: &adw::EntryRow| row.text().trim().parse::<f64>().ok();
            {
                let mut state = state.borrow_mut();
                state.config.weather_latitude = number(&latitude);
                state.config.weather_longitude = number(&longitude);
            }
            changed();
        }
    );
    latitude.connect_changed(clone!(
        #[strong]
        read,
        move |_| read()
    ));
    longitude.connect_changed(clone!(
        #[strong]
        read,
        move |_| read()
    ));

    behaviour.add(&mail_group(&state, &changed, current));

    let weather = adw::PreferencesGroup::builder()
        .title("Weather")
        .description("Where to get the weather for, as a decimal latitude and longitude — 40.0529, -83.0925. The forecast comes from the US National Weather Service, which needs no account and covers the United States only.")
        .build();
    weather.add(&latitude);
    weather.add(&longitude);
    behaviour.add(&weather);

    // On/off is a switch row and an hour is a number, so a spin row rather than
    // a combo of twenty-four — the same pair the compaction group above uses,
    // and for the same reason: the second row only means anything when the
    // first is on, so it goes insensitive with it rather than sitting there
    // doing nothing.
    let passive = adw::SwitchRow::builder()
        .title("Remember What You Mention")
        .subtitle("Read each finished turn for durable facts and save them without being asked")
        .active(current.settings.passive_memory)
        .build();
    passive.connect_active_notify(clone!(
        #[strong]
        state,
        #[strong]
        changed,
        move |row| {
            state.borrow_mut().settings.passive_memory = row.is_active();
            changed();
        }
    ));

    let dreaming = adw::SwitchRow::builder()
        .title("Tidy Up Overnight")
        .subtitle("Merge what is said twice, refile what is misfiled, and drop what has gone cold")
        .active(current.settings.dreaming)
        .build();

    let hour = adw::SpinRow::with_range(0.0, 23.0, 1.0);
    hour.set_title("Hour");
    hour.set_subtitle("Local time. A night the machine slept through is skipped, not caught up");
    hour.set_value(f64::from(current.settings.dream_hour));
    hour.set_sensitive(current.settings.dreaming);

    dreaming.connect_active_notify(clone!(
        #[strong]
        state,
        #[strong]
        changed,
        #[weak]
        hour,
        move |row| {
            state.borrow_mut().settings.dreaming = row.is_active();
            hour.set_sensitive(row.is_active());
            changed();
        }
    ));
    hour.connect_changed(clone!(
        #[strong]
        state,
        #[strong]
        changed,
        move |row| {
            state.borrow_mut().settings.dream_hour = row.value() as u32;
            changed();
        }
    ));

    // The vault last, and the "next launch" caveat with it rather than on the
    // group: it is true of where the notes are and false of everything else
    // here, which takes effect on the next turn.
    vault.set_subtitle(&format!(
        "{} — takes effect at the next launch",
        vault.subtitle().unwrap_or_default()
    ));

    let memory = adw::PreferencesGroup::builder()
        .title("Memory")
        .description(
            "Familiar remembers by writing Markdown into the same folder Brain uses. Nothing \
             is ever deleted except lines it wrote itself, and what a tidy-up removed is kept \
             where you can read it back.",
        )
        .build();
    memory.add(&passive);
    memory.add(&dreaming);
    memory.add(&hour);
    memory.add(&vault);
    behaviour.add(&memory);
    dialog.add(&behaviour);

    dialog.present(Some(parent));
}
