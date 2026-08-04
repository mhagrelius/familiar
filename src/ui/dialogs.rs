//! The dialogs, in one place because they are all the same shape.
//!
//! `AdwDialog` presents adaptively — centred on a desktop, a bottom sheet when
//! the window is narrow — and attaches with `present(parent)`. Every dialog
//! here follows the writing rules: Cancel first and the specific verb last,
//! header capitalisation on buttons, sentence capitalisation on descriptions,
//! and no "OK" anywhere.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib::{self, clone};

use crate::model::project::Project;

/// Ask for a name. Used for a new project, for renaming a chat, and for making
/// and renaming files.
///
/// The entry starts selected and Enter accepts, because typing a name and
/// pressing Return is the whole interaction.
pub fn ask_name<F>(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    verb: &str,
    initial: &str,
    on_accept: F,
) where
    F: Fn(String) + 'static,
{
    let dialog = adw::AlertDialog::new(Some(heading), None);
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("accept", verb);
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("accept"));
    dialog.set_close_response("cancel");

    let entry = adw::EntryRow::builder().title("Name").build();
    entry.set_text(initial);

    let group = adw::PreferencesGroup::new();
    group.add(&entry);
    dialog.set_extra_child(Some(&group));

    // An empty name is not a name; the accept button says so rather than the
    // dialog failing after the fact.
    let sensitivity = clone!(
        #[weak]
        dialog,
        #[weak]
        entry,
        move || {
            dialog.set_response_enabled("accept", !entry.text().trim().is_empty());
        }
    );
    entry.connect_changed(clone!(
        #[strong]
        sensitivity,
        move |_| sensitivity()
    ));
    sensitivity();

    entry.connect_entry_activated(clone!(
        #[weak]
        dialog,
        move |entry| {
            if !entry.text().trim().is_empty() {
                dialog.close();
                // The response is emitted by hand: activating the entry is the
                // same intent as pressing the default button.
                dialog.emit_by_name::<()>("response", &[&"accept"]);
            }
        }
    ));

    dialog.connect_response(
        None,
        clone!(
            #[weak]
            entry,
            move |_, response| {
                if response == "accept" {
                    let name = entry.text().trim().to_string();
                    if !name.is_empty() {
                        on_accept(name);
                    }
                }
            }
        ),
    );

    dialog.present(Some(parent));
    entry.grab_focus();
}

/// Confirm something that cannot be undone.
///
/// For destructive actions with no undo path — deleting a project takes its
/// chats with it, and there is no single file to put back.
pub fn confirm_destructive<F>(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    body: &str,
    verb: &str,
    on_confirm: F,
) where
    F: Fn() + 'static,
{
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("confirm", verb);
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |_, response| {
        if response == "confirm" {
            on_confirm();
        }
    });
    dialog.present(Some(parent));
}

/// What a project *is*: its name, what you have asked the assistant to do
/// inside it, the folder it works in, and which tools it offers.
///
/// The default project comes through here too. It has no name to change and no
/// project to delete, so those rows are simply not built — what is left is the
/// instructions and the tools, which is how somebody changes the assistant's
/// ordinary behaviour.
pub fn edit_project<F>(parent: &impl IsA<gtk::Widget>, project: &Project, on_save: F)
where
    F: Fn(Project) + 'static,
{
    let default = project.is_default();

    let dialog = adw::Dialog::new();
    dialog.set_title(if default {
        "Chat Settings"
    } else {
        "Project Settings"
    });
    dialog.set_content_width(520);
    dialog.set_content_height(660);

    let name = adw::EntryRow::builder().title("Name").build();
    name.set_text(&project.name);

    let identity = adw::PreferencesGroup::builder().build();
    identity.add(&name);

    let instructions = gtk::TextView::new();
    instructions.set_wrap_mode(gtk::WrapMode::WordChar);
    instructions.set_top_margin(8);
    instructions.set_bottom_margin(8);
    instructions.set_left_margin(8);
    instructions.set_right_margin(8);
    instructions
        .buffer()
        .set_text(project.instructions.as_deref().unwrap_or(""));

    let instructions_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(160)
        .child(&instructions)
        .build();
    instructions_scroller.add_css_class("card");

    // "Added to" rather than "replaces", and it says so because the difference
    // is the whole point: asking to be called Matt should not cost the
    // paragraph that makes answers render properly.
    let instructions_group = adw::PreferencesGroup::builder()
        .title("Instructions")
        .description(if default {
            "Added to what Familiar already knows, in every chat that is not in a project. This \
             is where you change how it behaves by default — how to address you, how long an \
             answer should be, anything it should always assume."
        } else {
            "Added to what Familiar already knows, in every chat in this project. Say what this \
             project is and how you want it worked on."
        })
        .build();
    instructions_group.add(&instructions_scroller);

    let memory = adw::SwitchRow::builder()
        .title("Memory")
        .subtitle("Read and add to your notes")
        .active(project.tools.memory)
        .build();
    let web = adw::SwitchRow::builder()
        .title("Web")
        .subtitle("Search and read pages — the only thing that leaves this machine")
        .active(project.tools.web)
        .build();
    let workspace = adw::SwitchRow::builder()
        .title("Files")
        .subtitle("Read what is in the folder, and write to it with your approval")
        .active(project.tools.workspace)
        .build();

    let weather = adw::SwitchRow::builder()
        .title("Weather")
        .subtitle("Current conditions and the forecast for your location")
        .active(project.tools.weather)
        .build();

    // Its own authority, and a big one: `gh` is signed in as the user and acts
    // on repositories other people share. Nested under Files because the
    // repository it acts on is the folder it runs in.
    let github = adw::SwitchRow::builder()
        .title("GitHub")
        .subtitle("Use the gh CLI — reading is free, changes need your approval")
        .active(project.tools.github)
        .sensitive(project.tools.workspace)
        .build();

    // Nested under Files because that is where the documents go: a document
    // switch on with the folder off would offer tools with nowhere to write.
    let documents = adw::SwitchRow::builder()
        .title("Documents")
        .subtitle("Make Word, Excel, PowerPoint and PDF files in the folder")
        .active(project.tools.documents)
        .sensitive(project.tools.workspace)
        .build();
    // Not nested under Files: the sandbox has a directory of its own and works
    // without one. A folder only adds something for a script to read.
    let python = adw::SwitchRow::builder()
        .title("Python")
        .subtitle("Run scripts in a container with no network, for exact answers")
        .active(project.tools.python)
        .build();
    // The only switch here that sends anything to a company's servers as a
    // matter of course, so the subtitle says so rather than describing the
    // benefit. Every call is gated on top of this.
    let mail = adw::SwitchRow::builder()
        .title("Mail")
        .subtitle("Read and organise your email — sending and deleting need your approval")
        .active(project.tools.mail)
        .build();
    // The capability already existed in the menu; this is the switch that lets
    // the assistant set one up for itself rather than the user doing it by
    // hand. Off by default like the rest, and here so that a schedule the
    // assistant made can be stopped where every other capability is stopped.
    let scheduling = adw::SwitchRow::builder()
        .title("Scheduling")
        .subtitle("Let a chat set itself to run on a schedule — you approve each one")
        .active(project.tools.scheduling)
        .build();
    // Sentence capitalization in the subtitle, and worded around what somebody
    // *wants* rather than what it is: "several steps" and "before it starts" are
    // the two things that distinguish this from the task list below it.
    let workflow = adw::SwitchRow::builder()
        .title("Workflows")
        .subtitle("Plan a job of several steps and work through it — you see the plan first")
        .active(project.tools.workflow)
        .build();
    let escalate = adw::SwitchRow::builder()
        .title("Ask a Stronger Model")
        .subtitle(
            "Send one question to Claude or Codex — leaves this machine, and you approve each one",
        )
        .active(project.tools.escalate)
        .build();
    // The sibling applications. Neither needs a folder — each keeps its own
    // store and its CLI talks to the running app — so neither is nested.
    let planner = adw::SwitchRow::builder()
        .title("Planner")
        .subtitle("Read and change your tasks — changes need your approval")
        .active(project.tools.planner)
        .build();

    let magpie = adw::SwitchRow::builder()
        .title("Magpie")
        .subtitle("Transcribe a video — downloading takes minutes and needs your approval")
        .active(project.tools.magpie)
        .build();

    workspace.connect_active_notify(clone!(
        #[weak]
        documents,
        #[weak]
        github,
        move |workspace| {
            documents.set_sensitive(workspace.is_active());
            github.set_sensitive(workspace.is_active());
        }
    ));

    // "Location" under a group already headed "Folder": the row said "Folder"
    // too, and a heading repeated immediately underneath itself is a heading
    // that says nothing.
    let folder = adw::ActionRow::builder()
        .title("Location")
        .subtitle(
            project
                .workspace
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "Not chosen".into()),
        )
        .build();

    let chosen: Rc<RefCell<Option<std::path::PathBuf>>> =
        Rc::new(RefCell::new(project.workspace.clone()));

    // Clearing is its own button rather than a third state on the chooser: a
    // folder that was chosen by mistake has to be removable, and there is
    // otherwise no way back to none.
    let clear = gtk::Button::from_icon_name("edit-clear-symbolic");
    clear.set_tooltip_text(Some("Use No Folder"));
    clear.set_valign(gtk::Align::Center);
    clear.add_css_class("flat");
    clear.set_sensitive(project.workspace.is_some());

    let choose = gtk::Button::with_label("Choose…");
    choose.set_valign(gtk::Align::Center);
    choose.connect_clicked(clone!(
        #[weak]
        folder,
        #[weak]
        workspace,
        #[weak]
        clear,
        #[weak]
        dialog,
        #[strong]
        chosen,
        move |_| {
            let picker = gtk::FileDialog::builder().title("Choose a Folder").build();
            picker.select_folder(
                dialog.root().and_downcast_ref::<gtk::Window>(),
                gtk::gio::Cancellable::NONE,
                clone!(
                    #[weak]
                    folder,
                    #[weak]
                    workspace,
                    #[weak]
                    clear,
                    #[strong]
                    chosen,
                    move |result| {
                        let Ok(picked) = result else { return };
                        let Some(path) = picked.path() else { return };
                        folder.set_subtitle(&path.display().to_string());
                        chosen.replace(Some(path));
                        clear.set_sensitive(true);
                        // Choosing a folder is what switching it on means.
                        workspace.set_active(true);
                    }
                ),
            );
        }
    ));
    clear.connect_clicked(clone!(
        #[weak]
        folder,
        #[strong]
        chosen,
        move |clear| {
            chosen.replace(None);
            folder.set_subtitle("Not chosen");
            clear.set_sensitive(false);
        }
    ));
    folder.add_suffix(&clear);
    folder.add_suffix(&choose);

    // The folder is what makes a project a project, so it sits above the tools
    // rather than among them — and the two switches that need one say so.
    let place = adw::PreferencesGroup::builder()
        .title("Folder")
        .description(if default {
            "Chats can have a folder too. Files in it appear in the sidebar, and the assistant \
             can read them once Files is on."
        } else {
            "The folder this project is about. Files in it appear in the sidebar, and the \
             assistant can read them once Files is on."
        })
        .build();
    place.add(&folder);

    let tools = adw::PreferencesGroup::builder()
        .title("Tools")
        .description(if default {
            "What the assistant can reach for in a chat that is not in a project"
        } else {
            "What the assistant can reach for in this project"
        })
        .build();
    // The four that are on out of the box come first, so the list reads as
    // "what it already does" before "what you can add".
    tools.add(&memory);
    tools.add(&web);
    tools.add(&weather);
    tools.add(&workflow);
    tools.add(&workspace);
    tools.add(&documents);
    tools.add(&github);
    tools.add(&python);
    tools.add(&mail);
    tools.add(&scheduling);
    tools.add(&escalate);
    tools.add(&planner);
    tools.add(&magpie);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(12);
    content.set_margin_bottom(24);
    content.set_margin_start(12);
    content.set_margin_end(12);
    if !default {
        content.append(&identity);
    }
    content.append(&instructions_group);
    content.append(&place);
    content.append(&tools);

    let clamp = adw::Clamp::builder()
        .maximum_size(520)
        .child(&content)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build();

    let cancel = gtk::Button::with_label("Cancel");
    cancel.connect_clicked(clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));

    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");

    let header = adw::HeaderBar::builder()
        .show_end_title_buttons(false)
        .show_start_title_buttons(false)
        .build();
    header.pack_start(&cancel);
    header.pack_end(&save);

    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&scroller));
    dialog.set_child(Some(&view));

    let slug = project.slug.clone();
    let version = project.version;
    let model = project.model.clone();
    let fallback_name = project.name.clone();
    save.connect_clicked(clone!(
        #[weak]
        dialog,
        // Strong, unlike every other row here, because this is the one that is
        // not always in the tree: the default project has no name to change, so
        // nothing else holds this row and a weak reference would already have
        // dropped — which made Save do nothing at all.
        #[strong]
        name,
        #[weak]
        instructions,
        #[weak]
        memory,
        #[weak]
        web,
        #[weak]
        workspace,
        #[weak]
        documents,
        #[weak]
        github,
        #[weak]
        python,
        #[weak]
        mail,
        #[weak]
        escalate,
        #[weak]
        planner,
        #[weak]
        magpie,
        #[weak]
        weather,
        #[strong]
        chosen,
        move |_| {
            let buffer = instructions.buffer();
            let written = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                .trim()
                .to_string();
            // The default project's name row is never built, so its name comes
            // back as it went in rather than as an empty string.
            let named = match default {
                true => fallback_name.clone(),
                false => name.text().trim().to_string(),
            };
            let edited = Project {
                version,
                slug: slug.clone(),
                name: named,
                instructions: (!written.is_empty()).then_some(written),
                tools: crate::model::project::ToolSet {
                    memory: memory.is_active(),
                    web: web.is_active(),
                    workspace: workspace.is_active(),
                    weather: weather.is_active(),
                    github: github.is_active(),
                    documents: documents.is_active(),
                    planner: planner.is_active(),
                    magpie: magpie.is_active(),
                    python: python.is_active(),
                    escalate: escalate.is_active(),
                    mail: mail.is_active(),
                    scheduling: scheduling.is_active(),
                    workflow: workflow.is_active(),
                },
                workspace: chosen.borrow().clone(),
                model: model.clone(),
            };
            if edited.name.is_empty() {
                name.grab_focus();
                return;
            }
            on_save(edited);
            dialog.close();
        }
    ));

    dialog.present(Some(parent));
}

/// One scheduled chat, as the management window shows it.
pub struct Scheduled {
    pub slug: String,
    pub project: String,
    pub thread: String,
    pub title: String,
    pub schedule: String,
    pub prompt: String,
    pub enabled: bool,
    /// "Ran 20 minutes ago", or why it has not.
    pub status: String,
    /// The cadence and standing prompt as values rather than as the sentences
    /// above them, so the editor can open pre-filled with what is actually set.
    /// `schedule` and `prompt` are what this row *reads* as; this is what it
    /// *is*.
    pub current: Option<(crate::model::heartbeat::Schedule, String)>,
}

/// What the window asks the application to do.
pub enum Change {
    Enabled {
        slug: String,
        thread: String,
        on: bool,
    },
    Deleted {
        slug: String,
        thread: String,
    },
    Opened {
        slug: String,
        thread: String,
    },
    /// A new cadence and standing prompt for a schedule, edited in place.
    ///
    /// The window that lists schedules is the window people go to when they want
    /// to change one, and it could only switch them off or delete them — the
    /// editor was a main-menu item that silently acted on whichever chat
    /// happened to be open. Setting one from the wrong chat did not fail or
    /// warn; it made a second schedule somewhere else.
    Edited {
        slug: String,
        thread: String,
        schedule: crate::model::heartbeat::Schedule,
        prompt: String,
    },
}

/// Every chat that wakes on its own, with a switch and a way out.
///
/// A schedule you cannot find is a schedule you cannot stop, and one that lives
/// only on the chat that owns it means hunting the sidebar for something you
/// may have set up weeks ago. So they are gathered in one place — which is also
/// the only place that can answer "did it run?", because that is a property of
/// the chat rather than of anything on screen.
pub fn present_schedules<F>(parent: &impl IsA<gtk::Widget>, scheduled: &[Scheduled], on_change: F)
where
    F: Fn(Change) + 'static,
{
    let dialog = adw::Dialog::new();
    dialog.set_title("Scheduled Chats");
    dialog.set_content_width(560);
    dialog.set_content_height(600);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(12);
    content.set_margin_bottom(24);
    content.set_margin_start(12);
    content.set_margin_end(12);

    if scheduled.is_empty() {
        // The empty state says how to get out of it: nothing here is
        // discoverable from a list of nothing.
        let empty = adw::StatusPage::builder()
            .icon_name("alarm-symbolic")
            .title("No Scheduled Chats")
            .description(
                "A chat can wake on its own and ask something for you — a morning briefing, \
                 a check on your pull requests. Open a chat and choose Schedule from the main \
                 menu.",
            )
            .build();
        empty.set_vexpand(true);
        content.append(&empty);
    }

    let on_change = Rc::new(on_change);
    for entry in scheduled {
        // The cadence has moved out of the description and into a row of its
        // own, because it is now something you can click to change rather than
        // a label. What is left up here is which project the chat belongs to.
        let group = adw::PreferencesGroup::builder()
            .title(&entry.title)
            .description(&entry.project)
            .build();

        let running = adw::SwitchRow::builder()
            .title("Enabled")
            .subtitle(&entry.status)
            .active(entry.enabled)
            .build();
        running.connect_active_notify({
            let on_change = on_change.clone();
            let slug = entry.slug.clone();
            let thread = entry.thread.clone();
            move |row| {
                on_change(Change::Enabled {
                    slug: slug.clone(),
                    thread: thread.clone(),
                    on: row.is_active(),
                });
            }
        });
        group.add(&running);

        // The two things a schedule *is*, each shown rather than hidden behind
        // an edit button — what a scheduled chat asks and when is the thing you
        // most want to check when you come looking for it — and each one a row
        // you can activate to change it. Two rows onto one editor rather than a
        // single Edit button, because whichever of them you came here to alter
        // is the one you will reach for, and a boxed list is read as a list of
        // fields.
        let when = adw::ActionRow::builder()
            .title("Schedule")
            .subtitle(&entry.schedule)
            .activatable(true)
            .build();
        when.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        group.add(&when);

        let prompt = adw::ActionRow::builder()
            .title("Prompt")
            .subtitle(&entry.prompt)
            .activatable(true)
            .build();
        prompt.set_subtitle_lines(3);
        prompt.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        group.add(&prompt);

        // Both rows open the same editor, pre-filled from this schedule, and
        // write back through `Change::Edited` — which goes to `edit_heartbeat`,
        // so it reaches the chat whether or not it is the one on screen. That
        // is the whole point: you should not have to find and open a chat to
        // change the thing you are already looking at.
        let edit = {
            let on_change = on_change.clone();
            let slug = entry.slug.clone();
            let thread = entry.thread.clone();
            let title = entry.title.clone();
            let existing = entry.current.clone();
            let dialog = dialog.clone();
            let when = when.clone();
            let prompt = prompt.clone();
            move || {
                let on_change = on_change.clone();
                let slug = slug.clone();
                let thread = thread.clone();
                let when = when.clone();
                let prompt = prompt.clone();
                edit_schedule(&dialog, &title, existing.clone(), move |chosen| {
                    let Some((schedule, asked)) = chosen else {
                        return;
                    };
                    // Written straight back into the rows, so the list a person
                    // is still looking at says what they just set rather than
                    // what it used to say.
                    when.set_subtitle(&schedule.describe());
                    prompt.set_subtitle(&asked);
                    on_change(Change::Edited {
                        slug: slug.clone(),
                        thread: thread.clone(),
                        schedule,
                        prompt: asked,
                    });
                });
            }
        };
        when.connect_activated({
            let edit = edit.clone();
            move |_| edit()
        });
        prompt.connect_activated(move |_| edit());

        let open = gtk::Button::with_label("Open");
        open.set_valign(gtk::Align::Center);
        open.connect_clicked({
            let on_change = on_change.clone();
            let slug = entry.slug.clone();
            let thread = entry.thread.clone();
            let dialog = dialog.clone();
            move |_| {
                on_change(Change::Opened {
                    slug: slug.clone(),
                    thread: thread.clone(),
                });
                dialog.close();
            }
        });

        let remove = gtk::Button::with_label("Remove Schedule");
        remove.set_valign(gtk::Align::Center);
        remove.add_css_class("destructive-action");
        remove.connect_clicked({
            let on_change = on_change.clone();
            let slug = entry.slug.clone();
            let thread = entry.thread.clone();
            let dialog = dialog.clone();
            move |button| {
                // The chat and its answers stay; only the schedule goes.
                // Destructive enough to be styled, cheap enough not to be a
                // confirmation — the prompt is one dialog away from being set
                // again, and the conversation is untouched.
                on_change(Change::Deleted {
                    slug: slug.clone(),
                    thread: thread.clone(),
                });
                button.set_sensitive(false);
                dialog.close();
            }
        });

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        buttons.set_halign(gtk::Align::End);
        buttons.append(&open);
        buttons.append(&remove);
        let actions = adw::ActionRow::new();
        actions.add_suffix(&buttons);
        group.add(&actions);

        content.append(&group);
    }

    let clamp = adw::Clamp::builder()
        .maximum_size(520)
        .child(&content)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build();

    let header = adw::HeaderBar::new();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&scroller));
    dialog.set_child(Some(&view));
    dialog.present(Some(parent));
}

/// Set or change when a chat wakes, and what it asks.
///
/// A picker of the four shapes people actually use rather than a cron field:
/// "weekdays at 07:00" is the thing being expressed, and a text box that
/// accepts `0 7 * * 1-5` asks the user to encode it and then to read it back
/// later. Every product in this space that started with cron has ended up
/// shipping this list.
/// `chat` is the name of the conversation being scheduled, and it is shown
/// rather than implied. Reached from the main menu this edits whichever chat is
/// open, and there was nothing on screen to say which — so a schedule set from
/// the wrong chat did not fail, it quietly made a second one somewhere else.
pub fn edit_schedule<F>(
    parent: &impl IsA<gtk::Widget>,
    chat: &str,
    existing: Option<(crate::model::heartbeat::Schedule, String)>,
    on_save: F,
) where
    F: Fn(Option<(crate::model::heartbeat::Schedule, String)>) + 'static,
{
    use crate::model::heartbeat::Schedule;

    let dialog = adw::Dialog::new();
    dialog.set_title("Schedule");
    dialog.set_content_width(520);
    dialog.set_content_height(560);

    let kinds = gtk::StringList::new(&["Every few hours", "Daily", "Weekdays", "Weekly"]);
    let kind = adw::ComboRow::builder()
        .title("Repeat")
        .model(&kinds)
        .build();

    let hours = adw::SpinRow::with_range(1.0, 24.0, 1.0);
    hours.set_title("Hours between runs");
    hours.set_value(6.0);

    let time = adw::EntryRow::builder().title("Time (HH:MM)").build();
    time.set_text("07:00");

    let days = gtk::StringList::new(&[
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
    ]);
    let weekday = adw::ComboRow::builder().title("Day").model(&days).build();

    // Fill from what is already set, so opening this on an existing schedule
    // shows it rather than a default that would overwrite it on save.
    let prompt_text = existing
        .as_ref()
        .map(|(_, prompt)| prompt.clone())
        .unwrap_or_default();
    match existing.as_ref().map(|(schedule, _)| *schedule) {
        Some(Schedule::Hours { hours: n }) => {
            kind.set_selected(0);
            hours.set_value(f64::from(n));
        }
        Some(Schedule::Daily { at }) => {
            kind.set_selected(1);
            time.set_text(&at.format("%H:%M").to_string());
        }
        Some(Schedule::Weekdays { at }) => {
            kind.set_selected(2);
            time.set_text(&at.format("%H:%M").to_string());
        }
        Some(Schedule::Weekly { day, at }) => {
            kind.set_selected(3);
            time.set_text(&at.format("%H:%M").to_string());
            weekday.set_selected(day.num_days_from_monday());
        }
        None => kind.set_selected(1),
    }

    // Only the rows that apply to the chosen shape. A greyed-out day picker
    // under "Daily" is a question the user has to work out is irrelevant.
    let relevant = clone!(
        #[weak]
        kind,
        #[weak]
        hours,
        #[weak]
        time,
        #[weak]
        weekday,
        move || {
            let selected = kind.selected();
            hours.set_visible(selected == 0);
            time.set_visible(selected != 0);
            weekday.set_visible(selected == 3);
        }
    );
    kind.connect_selected_notify(clone!(
        #[strong]
        relevant,
        move |_| relevant()
    ));
    relevant();

    let when = adw::PreferencesGroup::builder().title("When").build();
    when.add(&kind);
    when.add(&hours);
    when.add(&time);
    when.add(&weekday);

    let prompt = gtk::TextView::new();
    prompt.set_wrap_mode(gtk::WrapMode::WordChar);
    prompt.set_top_margin(8);
    prompt.set_bottom_margin(8);
    prompt.set_left_margin(8);
    prompt.set_right_margin(8);
    prompt.buffer().set_text(&prompt_text);
    let prompt_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(140)
        .child(&prompt)
        .build();
    prompt_scroller.add_css_class("card");

    let asking = adw::PreferencesGroup::builder()
        .title("Ask")
        .description("Submitted to this chat when it wakes, as though you had typed it. The chat keeps its history, so it can refer back to what it found last time.")
        .build();
    asking.add(&prompt_scroller);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(12);
    content.set_margin_bottom(24);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&when);
    content.append(&asking);

    let clamp = adw::Clamp::builder()
        .maximum_size(480)
        .child(&content)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build();

    let cancel = gtk::Button::with_label("Cancel");
    cancel.connect_clicked(clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));

    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");

    let on_save = Rc::new(on_save);
    save.connect_clicked(clone!(
        #[weak]
        dialog,
        #[weak]
        kind,
        #[weak]
        hours,
        #[weak]
        time,
        #[weak]
        weekday,
        #[weak]
        prompt,
        #[strong]
        on_save,
        move |_| {
            let buffer = prompt.buffer();
            let asked = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                .trim()
                .to_string();
            if asked.is_empty() {
                // A schedule with nothing to ask would wake the chat and
                // submit an empty turn.
                prompt.grab_focus();
                return;
            }
            let at = chrono::NaiveTime::parse_from_str(time.text().trim(), "%H:%M").unwrap_or_else(
                |_| chrono::NaiveTime::from_hms_opt(7, 0, 0).expect("a valid time"),
            );
            let schedule = match kind.selected() {
                0 => Schedule::Hours {
                    hours: hours.value() as u32,
                },
                2 => Schedule::Weekdays { at },
                3 => Schedule::Weekly {
                    day: WEEK[weekday.selected().min(6) as usize],
                    at,
                },
                _ => Schedule::Daily { at },
            };
            on_save(Some((schedule, asked)));
            dialog.close();
        }
    ));

    let header = adw::HeaderBar::builder()
        .show_end_title_buttons(false)
        .show_start_title_buttons(false)
        .build();
    header.set_title_widget(Some(&adw::WindowTitle::new("Schedule", chat)));
    header.pack_start(&cancel);
    header.pack_end(&save);

    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&scroller));
    dialog.set_child(Some(&view));
    dialog.present(Some(parent));
}

/// Rewrite a workflow's steps before — or during — the run.
///
/// The affordance the whole steering story rests on, so what it can and cannot
/// touch is the design rather than an implementation detail:
///
/// * **Steps already settled are shown and not editable.** Rewriting a step the
///   assistant has already reported on would leave the record disagreeing with
///   the conversation. `Workflow::revise` keeps the same line on the model's
///   side.
/// * **Notes are only ever written here.** The model never writes one and never
///   overwrites one; this dialog is the only door.
/// * The goal is editable, because a plan whose point has moved is worth
///   renaming rather than starting again.
///
/// Reordering is up/down buttons rather than drag-and-drop: at most a dozen
/// rows, and a keyboard user can reach these.
pub fn edit_workflow<F>(
    parent: &impl IsA<gtk::Widget>,
    workflow: &crate::model::workflow::Workflow,
    on_save: F,
) where
    F: Fn(crate::model::workflow::Workflow) + 'static,
{
    use crate::model::workflow::Step;

    let dialog = adw::Dialog::new();
    dialog.set_title("Workflow");
    dialog.set_content_width(560);
    dialog.set_content_height(640);

    let settled = workflow.current().unwrap_or(workflow.steps.len());
    // The editable tail, held apart from the widgets so a reorder is a move in
    // this list and a redraw, rather than widget surgery.
    let editing: Rc<RefCell<Vec<Step>>> = Rc::new(RefCell::new(workflow.steps[settled..].to_vec()));

    let goal = adw::EntryRow::builder().title("Goal").build();
    goal.set_text(&workflow.goal);
    let about = adw::PreferencesGroup::builder().title("Workflow").build();
    about.add(&goal);

    let done = adw::PreferencesGroup::builder()
        .title("Already done")
        .description("These have run. They are here so you can see what the rest builds on.")
        .build();
    for (index, step) in workflow.steps[..settled].iter().enumerate() {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&step.what))
            .subtitle(format!("Step {}", index + 1))
            .build();
        row.set_sensitive(false);
        done.add(&row);
    }
    done.set_visible(settled > 0);

    let steps = adw::PreferencesGroup::builder()
        .title("Steps")
        .description(
            "Change the wording, the order, or what is in the list. A note on a step is given \
             to the assistant when it gets there.",
        )
        .build();
    let rows = gtk::ListBox::new();
    rows.set_selection_mode(gtk::SelectionMode::None);
    rows.add_css_class("boxed-list");
    steps.add(&rows);

    // Added to the list rather than to the group. A plain `GtkListBox` is not one
    // of `AdwPreferencesGroup`'s rows, so a `ButtonRow` added beside it is not
    // ordered against it — and it rendered *above* the steps, which reads as
    // "add" being the first thing to do rather than the last. `draw_steps` puts
    // it back after every rebuild, so it stays at the bottom.
    let add = adw::ButtonRow::builder().title("Add Step").build();
    add.set_start_icon_name(Some("list-add-symbolic"));

    draw_steps(&rows, &add, &editing, settled);

    add.connect_activated({
        let editing = editing.clone();
        let rows = rows.clone();
        move |add| {
            editing.borrow_mut().push(Step::new("New step"));
            draw_steps(&rows, add, &editing, settled);
        }
    });

    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(12);
    content.set_margin_bottom(24);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&about);
    content.append(&done);
    content.append(&steps);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(
            &adw::Clamp::builder()
                .maximum_size(520)
                .child(&content)
                .build(),
        )
        .build();

    let cancel = gtk::Button::with_label("Cancel");
    cancel.connect_clicked(clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));

    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");

    let on_save = Rc::new(on_save);
    let before = workflow.clone();
    save.connect_clicked(clone!(
        #[weak]
        dialog,
        #[weak]
        goal,
        #[strong]
        editing,
        #[strong]
        on_save,
        move |_| {
            let mut kept: Vec<Step> = editing
                .borrow()
                .iter()
                .filter(|step| !step.what.trim().is_empty())
                .cloned()
                .collect();
            for step in &mut kept {
                step.what = step.what.trim().to_string();
            }

            let mut after = before.clone();
            let named = goal.text().trim().to_string();
            if !named.is_empty() {
                after.goal = named;
            }
            after.steps.truncate(settled);
            after.steps.extend(kept);
            // Deleting every remaining step is a way of saying stop, and the
            // strip has a button for that. Saving an empty tail would leave a
            // workflow that reports itself finished without having been.
            if after.steps.is_empty() {
                dialog.close();
                return;
            }
            after.edited = Some(crate::model::workflow::changes(&before, &after))
                .filter(|changed| !changed.is_empty());
            on_save(after);
            dialog.close();
        }
    ));

    let header = adw::HeaderBar::builder()
        .show_end_title_buttons(false)
        .show_start_title_buttons(false)
        .build();
    header.pack_start(&cancel);
    header.pack_end(&save);

    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&scroller));
    dialog.set_child(Some(&view));
    dialog.present(Some(parent));
}

/// Draw the editable steps, replacing whatever is there.
///
/// One rebuild per change, and a plain recursive function rather than a closure
/// that holds itself: the row handlers need to redraw the list they live in, and
/// the self-referential `Rc<RefCell<Option<Box<dyn Fn()>>>>` that expresses that
/// is both harder to read and something clippy is right to object to.
///
/// Rebuilt rather than patched because the row index *is* the position in the
/// list, and a reorder that moved widgets without renumbering them would leave
/// every handler writing to the wrong step.
fn draw_steps(
    rows: &gtk::ListBox,
    add: &adw::ButtonRow,
    editing: &Rc<RefCell<Vec<crate::model::workflow::Step>>>,
    settled: usize,
) {
    use crate::model::workflow::MAX_STEPS;

    rows.remove_all();
    let count = editing.borrow().len();
    for offset in 0..count {
        let step = editing.borrow()[offset].clone();

        let what = adw::EntryRow::builder()
            .title(format!("Step {}", settled + offset + 1))
            .build();
        what.set_text(&step.what);
        what.connect_changed({
            let editing = editing.clone();
            move |entry| {
                if let Some(step) = editing.borrow_mut().get_mut(offset) {
                    step.what = entry.text().to_string();
                }
            }
        });

        for (icon, tooltip, shift) in [
            ("go-up-symbolic", "Move Up", -1i64),
            ("go-down-symbolic", "Move Down", 1i64),
        ] {
            let button = gtk::Button::from_icon_name(icon);
            button.set_tooltip_text(Some(tooltip));
            button.add_css_class("flat");
            button.set_valign(gtk::Align::Center);
            button.set_sensitive(match shift {
                -1 => offset > 0,
                _ => offset + 1 < count,
            });
            button.connect_clicked({
                let editing = editing.clone();
                let rows = rows.clone();
                let add = add.clone();
                move |_| {
                    let to = (offset as i64 + shift) as usize;
                    editing.borrow_mut().swap(offset, to);
                    draw_steps(&rows, &add, &editing, settled);
                }
            });
            what.add_suffix(&button);
        }

        let remove = gtk::Button::from_icon_name("list-remove-symbolic");
        remove.set_tooltip_text(Some("Remove Step"));
        remove.add_css_class("flat");
        remove.set_valign(gtk::Align::Center);
        remove.connect_clicked({
            let editing = editing.clone();
            let rows = rows.clone();
            let add = add.clone();
            move |_| {
                editing.borrow_mut().remove(offset);
                draw_steps(&rows, &add, &editing, settled);
            }
        });
        what.add_suffix(&remove);
        rows.append(&what);

        // The note is its own row under the step, headed as theirs. It is the
        // one thing on this screen the assistant never writes, and the label
        // says so rather than leaving them to work it out.
        let note = adw::EntryRow::builder()
            .title("Your note for this step")
            .build();
        note.set_text(step.note.as_deref().unwrap_or_default());
        note.connect_changed({
            let editing = editing.clone();
            move |entry| {
                if let Some(step) = editing.borrow_mut().get_mut(offset) {
                    let written = entry.text().to_string();
                    step.note = (!written.trim().is_empty()).then(|| written.trim().to_string());
                }
            }
        });
        rows.append(&note);
    }
    add.set_sensitive(settled + count < MAX_STEPS);
    // Last, and re-appended because `remove_all` above unparented it.
    rows.append(add);
}

const WEEK: [chrono::Weekday; 7] = [
    chrono::Weekday::Mon,
    chrono::Weekday::Tue,
    chrono::Weekday::Wed,
    chrono::Weekday::Thu,
    chrono::Weekday::Fri,
    chrono::Weekday::Sat,
    chrono::Weekday::Sun,
];
