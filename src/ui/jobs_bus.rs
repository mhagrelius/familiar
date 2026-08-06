//! The job list, over D-Bus.
//!
//! **The interface is the asset, not any one surface.** A panel applet, a
//! `StatusNotifierItem` tray, a shell extension and a shell script all want the
//! same four things — what is scheduled, when it next runs, what happened last
//! time, and the ability to pause or run one — and none of them should be able
//! to reach into the application to get it. So this is the boundary, and the
//! surfaces are interchangeable behind it.
//!
//! Exported on the connection `GApplication` already owns, under the bus name
//! it already holds, which is why this costs a vtable rather than a subsystem.
//! `llama-tray` gets the same economy by running systemd, `StatusNotifierItem`
//! and `dbusmenu` over one `gio` connection.
//!
//! **Push, never poll.** `tailscale-gnome` holds the daemon's watch stream open
//! precisely so it has no timer, and a panel that woke every two seconds to ask
//! a local model app "anything scheduled?" is the version of this that ages
//! badly. Callers watch `PropertiesChanged` on `Jobs` and are told.

use gtk::gio;
use gtk::glib::{self, Variant};
use gtk::prelude::*;

/// The interface, as XML, because that is what `register_object` takes.
///
/// Deliberately small. Everything here answers a question a panel row asks; a
/// surface that wants to *edit* a schedule opens the app, because an editor is
/// a dialog and a dialog belongs where the rest of them are.
const INTERFACE: &str = r#"
<node>
  <interface name="us.hagreli.Familiar.Jobs">
    <method name="List">
      <arg type="aa{sv}" name="jobs" direction="out"/>
    </method>
    <method name="SetEnabled">
      <arg type="s" name="id" direction="in"/>
      <arg type="b" name="on" direction="in"/>
      <arg type="b" name="found" direction="out"/>
    </method>
    <method name="RunNow">
      <arg type="s" name="id" direction="in"/>
      <arg type="b" name="started" direction="out"/>
    </method>
    <property name="Jobs" type="aa{sv}" access="read"/>
    <property name="Overdue" type="u" access="read"/>
  </interface>
</node>
"#;

/// One job as the bus describes it.
///
/// A dictionary rather than a struct signature so a field can be added without
/// breaking every caller — a panel extension is versioned separately from the
/// app and the two will not update together.
pub fn describe(job: &crate::model::jobs::Job, now: chrono::DateTime<chrono::Local>) -> Variant {
    let next = job
        .next_run(now)
        .map(|when| when.to_rfc3339())
        .unwrap_or_default();
    let dict = glib::VariantDict::new(None);
    dict.insert("id", job.id.as_str());
    dict.insert("title", job.title().as_str());
    dict.insert("schedule", job.schedule.describe().as_str());
    dict.insert("prompt", job.prompt.as_str());
    dict.insert("enabled", job.enabled);
    dict.insert("recovery", job.recovery.describe());
    dict.insert("next_run", next.as_str());
    dict.insert(
        "last_outcome",
        job.last_outcome.clone().unwrap_or_default().as_str(),
    );
    dict.insert(
        "last_run",
        job.last_run
            .map(|at| at.to_rfc3339())
            .unwrap_or_default()
            .as_str(),
    );
    dict.insert("project", job.destination.slug().unwrap_or_default());
    dict.insert("chat", job.destination.thread().unwrap_or_default());
    dict.end()
}

/// Every job, in the shape the `Jobs` property has.
pub fn describe_all(
    jobs: &crate::model::jobs::Jobs,
    now: chrono::DateTime<chrono::Local>,
) -> Variant {
    let described: Vec<Variant> = jobs
        .jobs
        .iter()
        // The system's own upkeep is not somebody's schedule and has no row.
        .filter(|job| job.source.editable())
        .map(|job| describe(job, now))
        .collect();
    Variant::array_from_iter_with_type(
        glib::VariantTy::new("a{sv}").expect("a valid type"),
        described,
    )
}

/// How many jobs are due right now, which is what an icon can show without
/// anybody opening a menu.
pub fn overdue(jobs: &crate::model::jobs::Jobs, now: chrono::DateTime<chrono::Local>) -> u32 {
    jobs.jobs
        .iter()
        .filter(|job| job.source.editable() && job.due(now).is_some())
        .count() as u32
}

/// What the bus asks the application to do.
pub enum Ask {
    List,
    SetEnabled { id: String, on: bool },
    RunNow { id: String },
}

/// Export the interface on the application's own connection.
///
/// `handle` is called on the main loop for each request and answers it. The
/// registration id comes back so a caller could unexport; nothing does yet, and
/// the application outlives the bus in every case that matters.
pub fn export<F>(
    connection: &gio::DBusConnection,
    path: &str,
    handle: F,
) -> Result<gio::RegistrationId, glib::Error>
where
    F: Fn(Ask) -> Option<Variant> + 'static,
{
    let info = gio::DBusNodeInfo::for_xml(INTERFACE)?;
    let interface = info
        .lookup_interface("us.hagreli.Familiar.Jobs")
        .ok_or_else(|| {
            glib::Error::new(
                gio::IOErrorEnum::Failed,
                "the interface is not in its own XML",
            )
        })?;

    let handle = std::rc::Rc::new(handle);
    let for_method = handle.clone();
    let for_property = handle.clone();

    connection
        .register_object(path, &interface)
        .method_call(move |_, _, _, _, method, parameters, invocation| {
            let answer = match method {
                "List" => for_method(Ask::List),
                "SetEnabled" => {
                    let (id, on): (String, bool) = parameters.get().unwrap_or_default();
                    for_method(Ask::SetEnabled { id, on })
                }
                "RunNow" => {
                    let (id,): (String,) = parameters.get().unwrap_or_default();
                    for_method(Ask::RunNow { id })
                }
                _ => None,
            };
            match answer {
                // Every method returns exactly one value, so the reply is a
                // one-tuple around it.
                Some(value) => invocation.return_value(Some(&Variant::tuple_from_iter([value]))),
                None => invocation
                    .return_error(gio::IOErrorEnum::InvalidArgument, "no such method or job"),
            }
        })
        // An empty array rather than an error when the application cannot
        // answer: a panel reading a property must get something it can iterate,
        // and "no jobs" is the honest answer for an app with none.
        .property(move |_, _, _, _, property| {
            let empty = || {
                Variant::array_from_iter_with_type(
                    glib::VariantTy::new("a{sv}").expect("a valid type"),
                    [] as [Variant; 0],
                )
            };
            match property {
                "Jobs" => for_property(Ask::List).unwrap_or_else(empty),
                // Counted from the same answer `List` gives, so the two can
                // never disagree about what is scheduled.
                "Overdue" => for_property(Ask::List)
                    .map(|jobs| jobs.n_children() as u32)
                    .unwrap_or(0)
                    .to_variant(),
                _ => empty(),
            }
        })
        .build()
}

/// Tell every watcher the list changed.
///
/// The push half of "push, never poll". Emitted whenever a job is added,
/// edited, paused, deleted or finishes a run — a surface that has watched
/// `PropertiesChanged` never needs a timer.
pub fn changed(connection: &gio::DBusConnection, path: &str, jobs: &Variant, overdue: u32) {
    let changed = glib::VariantDict::new(None);
    changed.insert("Jobs", jobs.clone());
    changed.insert("Overdue", overdue);
    let body = Variant::tuple_from_iter([
        "us.hagreli.Familiar.Jobs".to_variant(),
        changed.end(),
        // Nothing is invalidated: both properties are carried in full above, so
        // a watcher never has to come back and ask.
        Variant::array_from_iter_with_type(glib::VariantTy::STRING, [] as [Variant; 0]),
    ]);
    let _ = connection.emit_signal(
        None,
        path,
        "org.freedesktop.DBus.Properties",
        "PropertiesChanged",
        Some(&body),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::heartbeat::{Recovery, Schedule};
    use crate::model::jobs::{Destination, Job, Jobs, Source};

    fn at(hour: u32, minute: u32) -> chrono::NaiveTime {
        chrono::NaiveTime::from_hms_opt(hour, minute, 0).expect("a time")
    }

    fn briefing() -> Job {
        let mut job = Job::new(
            "morning",
            Schedule::Daily { at: at(7, 0) },
            "the morning briefing",
            Destination::Chat {
                slug: "default".into(),
                thread: "chat-1".into(),
            },
        );
        job.name = "Morning Briefing".into();
        job.recovery = Recovery::SameDay;
        job
    }

    #[test]
    fn the_interface_xml_parses() {
        // It is a string constant, so nothing else would catch a typo in it
        // until the first client tried to call a method.
        let info = gio::DBusNodeInfo::for_xml(INTERFACE).expect("the XML should parse");
        assert!(info.lookup_interface("us.hagreli.Familiar.Jobs").is_some());
    }

    #[test]
    fn a_job_describes_itself_as_a_dictionary_a_panel_can_read() {
        let now = chrono::Local::now();
        let described = describe(&briefing(), now);
        let text = described.to_string();
        assert!(text.contains("morning"), "{text}");
        assert!(text.contains("Morning Briefing"), "{text}");
        assert!(text.contains("Daily at 07:00"), "{text}");
        // The recovery reads as words, because a panel shows it verbatim.
        assert!(text.contains("Later the same day"), "{text}");
    }

    #[test]
    fn the_systems_own_upkeep_has_no_row_on_the_bus() {
        // Consolidation's cadence is in Preferences. A panel offering it as a
        // pausable job would be a second place for the same setting.
        let mut jobs = Jobs::default();
        jobs.add(briefing(), chrono::Utc::now());
        let mut upkeep = briefing();
        upkeep.id = "upkeep".into();
        upkeep.source = Source::System;
        jobs.add(upkeep, chrono::Utc::now());

        let described = describe_all(&jobs, chrono::Local::now());
        assert_eq!(described.n_children(), 1);
    }

    #[test]
    fn overdue_counts_only_what_is_actually_due() {
        use chrono::TimeZone;
        let local = |text: &str| {
            chrono::Local
                .from_local_datetime(
                    &chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").expect("a time"),
                )
                .earliest()
                .expect("a local time")
        };
        let mut jobs = Jobs::default();
        let mut owed = briefing();
        owed.recovery = Recovery::Whenever;
        owed.last_run = Some(local("2026-08-02 07:00").with_timezone(&chrono::Utc));
        jobs.add(owed, chrono::Utc::now());

        let mut settled = briefing();
        settled.id = "settled".into();
        settled.last_run = Some(local("2026-08-03 07:00").with_timezone(&chrono::Utc));
        jobs.add(settled, chrono::Utc::now());

        assert_eq!(overdue(&jobs, local("2026-08-03 09:00")), 1);
        // And a paused job is never overdue, however long it has been.
        for job in jobs.jobs.iter_mut() {
            job.enabled = false;
        }
        assert_eq!(overdue(&jobs, local("2026-08-03 09:00")), 0);
    }

    #[test]
    fn an_empty_list_is_an_empty_array_rather_than_nothing() {
        // A panel reading the property on a machine with no schedules must get
        // an array it can iterate, not a type error.
        let described = describe_all(&Jobs::default(), chrono::Local::now());
        assert_eq!(described.n_children(), 0);
    }
}
