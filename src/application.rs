/* application.rs
 *
 * Copyright 2026 Unknown
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

use gettextrs::gettext;
use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::config::VERSION;
use crate::SpeedometerWindow;

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct SpeedometerApplication {
        pub inhibit_cookie: std::cell::Cell<u32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SpeedometerApplication {
        const NAME: &'static str = "SpeedometerApplication";
        type Type = super::SpeedometerApplication;
        type ParentType = adw::Application;
    }

    impl ObjectImpl for SpeedometerApplication {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_gactions();
            obj.set_accels_for_action("app.quit", &["<control>q"]);
        }
    }

    impl ApplicationImpl for SpeedometerApplication {
        fn activate(&self) {
            let application = self.obj();
            // Get the current window or create one if necessary
            let window = application.active_window().unwrap_or_else(|| {
                let window = SpeedometerWindow::new(&*application);
                window.upcast()
            });

            // Inhibit suspend and idle so the screen stays on while navigating.
            // gtk::Application::inhibit() handles the XDG portal correctly and
            // requires a valid window handle, which we have at this point.
            if self.inhibit_cookie.get() == 0 {
                let cookie = application.inhibit(
                    Some(&window),
                    gtk::ApplicationInhibitFlags::SUSPEND | gtk::ApplicationInhibitFlags::IDLE,
                    Some("Speedometer is active"),
                );
                self.inhibit_cookie.set(cookie);
            }

            // Ask the window manager/compositor to present the window
            window.present();
        }

        fn shutdown(&self) {
            let cookie = self.inhibit_cookie.get();
            if cookie != 0 {
                self.obj().uninhibit(cookie);
                self.inhibit_cookie.set(0);
            }
            self.parent_shutdown();
        }
    }

    impl GtkApplicationImpl for SpeedometerApplication {}
    impl AdwApplicationImpl for SpeedometerApplication {}
}

glib::wrapper! {
    pub struct SpeedometerApplication(ObjectSubclass<imp::SpeedometerApplication>)
        @extends gio::Application, gtk::Application, adw::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl SpeedometerApplication {
    pub fn new(application_id: &str, flags: &gio::ApplicationFlags) -> Self {
        glib::Object::builder()
            .property("application-id", application_id)
            .property("flags", flags)
            .property("resource-base-path", "/io/github/nico359/speedometer")
            .build()
    }

    fn setup_gactions(&self) {
        let quit_action = gio::ActionEntry::builder("quit")
            .activate(move |app: &Self, _, _| app.quit())
            .build();
        let about_action = gio::ActionEntry::builder("about")
            .activate(move |app: &Self, _, _| app.show_about())
            .build();
        self.add_action_entries([quit_action, about_action]);
    }

    fn show_about(&self) {
        let window = self.active_window().unwrap();
        let about = adw::AboutDialog::builder()
            .application_name("Speedometer")
            .application_icon("io.github.nico359.speedometer")
            .developer_name("nico359")
            .version(VERSION)
            .developers(vec!["nico359", "GitHub Copilot CLI (Claude)"])
            .comments("A GPS speedometer for mobile Linux.\n\nBuilt with the assistance of AI (GitHub Copilot CLI, powered by Claude).")
            .website("https://github.com/nico359/speedometer")
            .issue_url("https://github.com/nico359/speedometer/issues")
            .license_type(gtk::License::Gpl30)
            .translator_credits(&gettext("translator-credits"))
            .copyright("© 2026 nico359")
            .build();

        about.add_credit_section(
            Some(&gettext("Inspired by")),
            &["Movens by wilfridd https://open-store.io/app/movens.wilfridd"],
        );

        about.add_legal_section(
            "Disclaimer",
            None,
            gtk::License::Custom,
            Some("For informational purposes only. Speed readings are GPS-based and approximate - not legally certified for any purpose. Do not use while driving. No warranty is given for accuracy or fitness for any particular purpose."),
        );

        about.present(Some(&window));
    }
}
