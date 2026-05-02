/* window.rs
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

use std::cell::Cell;
use std::f64::consts::PI;

use gtk::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

// ── Drawing constants ────────────────────────────────────────────────────────

/// Cairo angle (radians) at which the 0 km/h position sits.
/// Cairo angles: 0 = right, increase clockwise on screen.
/// 150° places the start at the lower-left (~8 o'clock).
const START_DEG: f64 = 150.0;

/// Total angular sweep of the dial (240° covers 8-o'clock → top → 4-o'clock).
const SWEEP_DEG: f64 = 240.0;

/// Maximum speed shown on the dial.
const MAX_SPEED: f64 = 200.0;

// ── Subclass ─────────────────────────────────────────────────────────────────

mod imp {
    use super::*;

    #[derive(Debug, Default, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/nico359/speedometer/window.ui")]
    pub struct SpeedometerWindow {
        #[template_child]
        pub speedometer_area: TemplateChild<gtk::DrawingArea>,

        pub speed: Cell<f64>,
        pub altitude: Cell<f64>,
        pub accuracy: Cell<f64>,
        pub has_fix: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SpeedometerWindow {
        const NAME: &'static str = "SpeedometerWindow";
        type Type = super::SpeedometerWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for SpeedometerWindow {
        fn constructed(&self) {
            self.parent_constructed();

            let obj = self.obj();

            // Wire up the DrawingArea draw callback.
            let win_weak = obj.downgrade();
            self.speedometer_area.set_draw_func(move |_, cr, width, height| {
                if let Some(win) = win_weak.upgrade() {
                    let imp = win.imp();
                    draw_speedometer(
                        imp.speed.get(),
                        imp.altitude.get(),
                        imp.accuracy.get(),
                        imp.has_fix.get(),
                        cr,
                        width,
                        height,
                    );
                }
            });

            // Create a glib channel so the GPS background thread can push
            // updates into the GTK main loop safely.
            let (sender, receiver) =
                async_channel::bounded::<crate::location::LocationData>(1);

            crate::location::start_location_watching(sender);

            let win_weak2 = obj.downgrade();
            glib::MainContext::default().spawn_local(async move {
                while let Ok(data) = receiver.recv().await {
                    if let Some(win) = win_weak2.upgrade() {
                        let imp = win.imp();
                        imp.speed.set(data.speed_kmh);
                        imp.altitude.set(data.altitude_m);
                        imp.accuracy.set(data.accuracy_m);
                        imp.has_fix.set(data.has_fix);
                        imp.speedometer_area.queue_draw();
                    }
                }
            });
        }
    }

    impl WidgetImpl for SpeedometerWindow {}
    impl WindowImpl for SpeedometerWindow {}
    impl ApplicationWindowImpl for SpeedometerWindow {}
    impl AdwApplicationWindowImpl for SpeedometerWindow {}
}

glib::wrapper! {
    pub struct SpeedometerWindow(ObjectSubclass<imp::SpeedometerWindow>)
        @extends gtk::Widget, gtk::Window, gtk::ApplicationWindow, adw::ApplicationWindow,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl SpeedometerWindow {
    pub fn new<P: IsA<gtk::Application>>(application: &P) -> Self {
        glib::Object::builder()
            .property("application", application)
            .build()
    }
}

// ── Cairo drawing ─────────────────────────────────────────────────────────────

/// Top-level draw function called on every frame.
fn draw_speedometer(
    speed: f64,
    altitude: f64,
    accuracy: f64,
    has_fix: bool,
    cr: &gtk::cairo::Context,
    width: i32,
    height: i32,
) {
    let w = width as f64;
    let h = height as f64;

    // Fit a square dial into the available area.
    let size = w.min(h);
    let cx = w / 2.0;
    let cy = h / 2.0;

    let r = size * 0.42;               // arc track radius
    let track_width = size * 0.038;    // arc stroke width

    let speed_clamped = speed.clamp(0.0, MAX_SPEED);
    let needle_rad = (START_DEG + (speed_clamped / MAX_SPEED) * SWEEP_DEG).to_radians();
    let start_rad = START_DEG.to_radians();
    // end_rad wraps: 150° + 240° = 390° → same as 30°
    let end_rad = (START_DEG + SWEEP_DEG).to_radians();

    // ── Window background ──────────────────────────────────────────────────
    cr.set_source_rgb(0.10, 0.10, 0.12);
    cr.paint().ok();

    // ── Dial background circle ─────────────────────────────────────────────
    cr.arc(cx, cy, r + track_width * 1.2, 0.0, 2.0 * PI);
    cr.set_source_rgb(0.07, 0.07, 0.09);
    cr.fill().ok();

    // ── Gray track (full 240° sweep) ───────────────────────────────────────
    cr.set_source_rgba(0.28, 0.28, 0.30, 0.7);
    cr.set_line_width(track_width);
    cr.arc(cx, cy, r, start_rad, end_rad);
    cr.stroke().ok();

    // ── Coloured speed arc (green → yellow → red) ─────────────────────────
    if speed_clamped > 0.1 {
        let ratio = speed_clamped / MAX_SPEED;
        let (red, green) = if ratio <= 0.5 {
            (ratio * 2.0, 1.0)
        } else {
            (1.0, 1.0 - (ratio - 0.5) * 2.0)
        };
        cr.set_source_rgba(red, green, 0.0, 0.9);
        cr.set_line_width(track_width);
        cr.arc(cx, cy, r, start_rad, needle_rad);
        cr.stroke().ok();
    }

    // ── Tick marks ────────────────────────────────────────────────────────
    cr.set_line_cap(gtk::cairo::LineCap::Round);

    let r_major_inner = r - size * 0.065;
    let r_minor_inner = r - size * 0.038;
    let r_label = r - size * 0.14;

    for v in (0u32..=200).step_by(10) {
        let t_rad = (START_DEG + (v as f64 / MAX_SPEED) * SWEEP_DEG).to_radians();
        let cos_t = t_rad.cos();
        let sin_t = t_rad.sin();

        let is_major = v % 20 == 0;
        let r_inner = if is_major { r_major_inner } else { r_minor_inner };

        cr.set_source_rgb(0.80, 0.80, 0.82);
        cr.set_line_width(if is_major { size * 0.013 } else { size * 0.007 });

        cr.move_to(cx + r_inner * cos_t, cy + r_inner * sin_t);
        cr.line_to(cx + r * cos_t, cy + r * sin_t);
        cr.stroke().ok();
    }

    // ── Speed labels (0, 40, 80, 120, 160, 200) ───────────────────────────
    cr.set_source_rgb(0.88, 0.88, 0.90);
    cr.set_font_size(size * 0.055);
    cr.select_font_face(
        "Sans",
        gtk::cairo::FontSlant::Normal,
        gtk::cairo::FontWeight::Normal,
    );

    for v in [0u32, 40, 80, 120, 160, 200] {
        let t_rad = (START_DEG + (v as f64 / MAX_SPEED) * SWEEP_DEG).to_radians();
        let lx = cx + r_label * t_rad.cos();
        let ly = cy + r_label * t_rad.sin();

        let label = v.to_string();
        if let Ok(ext) = cr.text_extents(&label) {
            cr.move_to(
                lx - ext.width() / 2.0 - ext.x_bearing(),
                ly + ext.height() / 2.0,
            );
            cr.show_text(&label).ok();
        }
    }

    // ── Needle ────────────────────────────────────────────────────────────
    let needle_cos = needle_rad.cos();
    let needle_sin = needle_rad.sin();

    // Shadow for depth
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.4);
    cr.set_line_width(size * 0.018);
    cr.set_line_cap(gtk::cairo::LineCap::Round);
    cr.move_to(cx - r * 0.18 * needle_cos + 2.0, cy - r * 0.18 * needle_sin + 2.0);
    cr.line_to(cx + r * 0.88 * needle_cos + 2.0, cy + r * 0.88 * needle_sin + 2.0);
    cr.stroke().ok();

    // Needle itself
    let (nr, ng, nb) = if has_fix {
        (0.92, 0.22, 0.22)
    } else {
        (0.55, 0.55, 0.55)
    };
    cr.set_source_rgb(nr, ng, nb);
    cr.set_line_width(size * 0.014);
    cr.move_to(cx - r * 0.18 * needle_cos, cy - r * 0.18 * needle_sin);
    cr.line_to(cx + r * 0.88 * needle_cos, cy + r * 0.88 * needle_sin);
    cr.stroke().ok();

    // Centre hub
    cr.arc(cx, cy, size * 0.028, 0.0, 2.0 * PI);
    cr.set_source_rgb(0.75, 0.75, 0.77);
    cr.fill().ok();
    cr.arc(cx, cy, size * 0.012, 0.0, 2.0 * PI);
    cr.set_source_rgb(0.30, 0.30, 0.32);
    cr.fill().ok();

    // ── Large speed readout ───────────────────────────────────────────────
    let speed_str = if has_fix {
        format!("{:.0}", speed_clamped)
    } else {
        "--".to_string()
    };

    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.set_font_size(size * 0.20);
    cr.select_font_face(
        "Sans",
        gtk::cairo::FontSlant::Normal,
        gtk::cairo::FontWeight::Bold,
    );
    if let Ok(ext) = cr.text_extents(&speed_str) {
        cr.move_to(
            cx - ext.width() / 2.0 - ext.x_bearing(),
            cy + size * 0.10,
        );
        cr.show_text(&speed_str).ok();
    }

    // "km/h" label
    cr.set_source_rgb(0.60, 0.62, 0.65);
    cr.set_font_size(size * 0.068);
    cr.select_font_face(
        "Sans",
        gtk::cairo::FontSlant::Normal,
        gtk::cairo::FontWeight::Normal,
    );
    if let Ok(ext) = cr.text_extents("km/h") {
        cr.move_to(cx - ext.width() / 2.0 - ext.x_bearing(), cy + size * 0.20);
        cr.show_text("km/h").ok();
    }

    // ── GPS status indicator ──────────────────────────────────────────────
    let dot_r = size * 0.022;
    // Use window height so the indicator sits near the top on portrait phones.
    let dot_cx = cx;
    let dot_cy = h * 0.08;

    cr.arc(dot_cx, dot_cy, dot_r, 0.0, 2.0 * PI);
    if !has_fix {
        cr.set_source_rgb(0.90, 0.20, 0.20);   // red  – no data
    } else if accuracy < 10.0 {
        cr.set_source_rgb(0.18, 0.85, 0.30);   // green – good fix
    } else {
        cr.set_source_rgb(0.95, 0.78, 0.10);   // yellow – coarse fix
    }
    cr.fill().ok();

    cr.set_source_rgb(0.55, 0.57, 0.60);
    cr.set_font_size(size * 0.042);
    cr.select_font_face(
        "Sans",
        gtk::cairo::FontSlant::Normal,
        gtk::cairo::FontWeight::Normal,
    );
    let gps_label = if has_fix {
        if accuracy >= 1000.0 {
            format!("±{:.1} km", accuracy / 1000.0)
        } else {
            format!("±{:.0} m", accuracy)
        }
    } else {
        "No signal".to_string()
    };
    // Centre the accuracy/status label horizontally below the dot.
    let line_gap = size * 0.052;
    let label_y = dot_cy + dot_r + line_gap;
    if let Ok(ext) = cr.text_extents(&gps_label) {
        cr.move_to(
            dot_cx - ext.width() / 2.0 - ext.x_bearing(),
            label_y,
        );
        cr.show_text(&gps_label).ok();
    }

    // Hint text on a second line.
    let hint: Option<&str> = if !has_fix {
        Some("Is GPS enabled?")
    } else if accuracy >= 10.0 {
        Some("Speed cannot be determined at this accuracy")
    } else {
        None
    };
    if let Some(hint_str) = hint {
        cr.set_font_size(size * 0.034);
        cr.set_source_rgb(0.45, 0.47, 0.50);
        if let Ok(ext) = cr.text_extents(hint_str) {
            cr.move_to(
                dot_cx - ext.width() / 2.0 - ext.x_bearing(),
                label_y + line_gap,
            );
            cr.show_text(hint_str).ok();
        }
    }

    // ── Altitude ──────────────────────────────────────────────────────────
    if has_fix {
        let alt_str = format!("{:.0} m", altitude);
        cr.set_source_rgb(0.50, 0.52, 0.55);
        cr.set_font_size(size * 0.048);
        cr.select_font_face(
            "Sans",
            gtk::cairo::FontSlant::Normal,
            gtk::cairo::FontWeight::Normal,
        );
        if let Ok(ext) = cr.text_extents(&alt_str) {
            cr.move_to(cx - ext.width() / 2.0 - ext.x_bearing(), cy + size * 0.30);
            cr.show_text(&alt_str).ok();
        }
    }
}

