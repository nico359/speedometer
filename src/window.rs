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

/// EMA smoothing factor per 16 ms tick (0 = frozen, 1 = instant).
/// 0.08 gives a ~120 ms half-life — reacts quickly then decelerates naturally.
const EMA_ALPHA: f64 = 0.08;

// ── Subclass ─────────────────────────────────────────────────────────────────

mod imp {
    use super::*;

    #[derive(Debug, Default, gtk::CompositeTemplate)]
    #[template(resource = "/io/github/nico359/speedometer/window.ui")]
    pub struct SpeedometerWindow {
        #[template_child]
        pub speedometer_area: TemplateChild<gtk::DrawingArea>,

        pub altitude: Cell<f64>,
        pub accuracy: Cell<f64>,
        pub has_fix: Cell<bool>,
        pub latitude: Cell<f64>,
        pub longitude: Cell<f64>,

        // Speed needle animation state (exponential moving average)
        pub speed_displayed: Cell<f64>,  // current smoothed value drawn on screen
        pub speed_target:    Cell<f64>,  // latest value from GPS

        // Time-to-fix tracking
        pub search_start_us: Cell<i64>,  // glib::monotonic_time() when search began
        pub time_to_fix_s:   Cell<i64>,  // seconds until fix was acquired; -1 = not yet
        pub prev_has_fix:    Cell<bool>, // last known fix state for transition detection
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

            // Start the search timer immediately on construction.
            self.search_start_us.set(glib::monotonic_time());
            self.time_to_fix_s.set(-1);

            // Wire up the DrawingArea draw callback.
            let win_weak = obj.downgrade();
            self.speedometer_area.set_draw_func(move |_, cr, width, height| {
                if let Some(win) = win_weak.upgrade() {
                    let imp = win.imp();
                    // Compute elapsed seconds using the same "good fix" threshold.
                    let good_fix = imp.has_fix.get() && imp.accuracy.get() < 10.0;
                    let elapsed_s = if !good_fix {
                        (glib::monotonic_time() - imp.search_start_us.get()) / 1_000_000
                    } else {
                        imp.time_to_fix_s.get()
                    };
                    draw_speedometer(
                        imp.speed_displayed.get(),
                        imp.altitude.get(),
                        imp.accuracy.get(),
                        imp.has_fix.get(),
                        imp.latitude.get(),
                        imp.longitude.get(),
                        elapsed_s,
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
                        // "Good fix" means signal acquired AND accuracy within 10 m
                        // (same threshold that turns the dot green).
                        let good_fix = data.has_fix && data.accuracy_m < 10.0;
                        let prev = imp.prev_has_fix.get();
                        // Detect fix transitions.
                        if prev && !good_fix {
                            // Lost good fix — restart the search timer.
                            imp.search_start_us.set(glib::monotonic_time());
                            imp.time_to_fix_s.set(-1);
                        } else if !prev && good_fix {
                            // Acquired good fix — record how long it took.
                            let elapsed = (glib::monotonic_time() - imp.search_start_us.get()) / 1_000_000;
                            imp.time_to_fix_s.set(elapsed);
                        }
                        imp.prev_has_fix.set(good_fix);
                        set_speed_target(imp, data.speed_kmh);
                        imp.altitude.set(data.altitude_m);
                        imp.accuracy.set(data.accuracy_m);
                        imp.has_fix.set(data.has_fix);
                        imp.latitude.set(data.latitude);
                        imp.longitude.set(data.longitude);
                        // The ticker drives redraws; no queue_draw() needed here.
                    }
                }
            });

            // 60 fps ticker: advance the EMA and redraw.
            let win_weak3 = obj.downgrade();
            glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
                if let Some(win) = win_weak3.upgrade() {
                    let imp = win.imp();
                    let displayed = imp.speed_displayed.get();
                    let target    = imp.speed_target.get();
                    // EMA: move a fixed fraction toward the target each tick.
                    imp.speed_displayed.set(displayed + (target - displayed) * EMA_ALPHA);
                    imp.speedometer_area.queue_draw();
                    glib::ControlFlow::Continue
                } else {
                    glib::ControlFlow::Break
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

// ── Speed animation helper ────────────────────────────────────────────────────

/// Update the needle animation target. The EMA ticker will smoothly
/// drive `speed_displayed` toward this value on every frame.
fn set_speed_target(imp: &imp::SpeedometerWindow, new_target: f64) {
    imp.speed_target.set(new_target);
}

// ── Cairo drawing ─────────────────────────────────────────────────────────────

/// Top-level draw function called on every frame.
/// Dispatches to portrait or landscape layout based on aspect ratio.
fn draw_speedometer(
    speed: f64,
    altitude: f64,
    accuracy: f64,
    has_fix: bool,
    latitude: f64,
    longitude: f64,
    elapsed_s: i64,
    cr: &gtk::cairo::Context,
    width: i32,
    height: i32,
) {
    let w = width as f64;
    let h = height as f64;

    cr.set_source_rgb(0.10, 0.10, 0.12);
    cr.paint().ok();

    if w > h {
        draw_landscape(speed, altitude, accuracy, has_fix, latitude, longitude, elapsed_s, cr, w, h);
    } else {
        draw_portrait(speed, altitude, accuracy, has_fix, latitude, longitude, elapsed_s, cr, w, h);
    }
}

/// Portrait layout: GPS indicator above the dial, info panel below.
fn draw_portrait(
    speed: f64,
    altitude: f64,
    accuracy: f64,
    has_fix: bool,
    latitude: f64,
    longitude: f64,
    elapsed_s: i64,
    cr: &gtk::cairo::Context,
    w: f64,
    h: f64,
) {
    // top_margin is sized generously so the GPS block (dot + two text lines)
    // never overlaps the dial even on near-square windows.
    let top_margin = h * 0.22;
    let bot_margin = h * 0.22;
    let usable_h   = h - top_margin - bot_margin;

    // size drives every dial dimension; cap it so the dial never overflows
    // into either margin, regardless of how wide the window gets.
    let size = w.min(usable_h / 0.84);   // 0.84 ≈ dial diameter / size

    let cx = w / 2.0;
    // Centre the dial in the usable vertical band.
    let cy = top_margin + usable_h / 2.0;

    draw_dial(speed, has_fix, cr, cx, cy, size);

    // GPS status indicator sits in the top margin band, anchored to top_margin
    // so it scales correctly instead of being fixed at h*0.05.
    let dot_cy = top_margin * 0.38;
    draw_gps_status(accuracy, has_fix, elapsed_s, cr, cx, dot_cy, size);

    // Info panel starts just below the dial background circle.
    let r           = size * 0.42;
    let track_width = size * 0.038;
    let panel_top = cy + r + track_width * 1.2 + size * 0.06;
    let row_gap   = size * 0.075;
    draw_info_labels(altitude, latitude, longitude, has_fix, cr, cx, panel_top, row_gap, size);
}

/// Landscape layout: dial on the left half, GPS / altitude / coordinates on the right.
fn draw_landscape(
    speed: f64,
    altitude: f64,
    accuracy: f64,
    has_fix: bool,
    latitude: f64,
    longitude: f64,
    elapsed_s: i64,
    cr: &gtk::cairo::Context,
    w: f64,
    h: f64,
) {
    let half_w = w / 2.0;

    // Dial fills the left half, vertically centred.
    let cx_dial = half_w / 2.0;
    let cy_dial = h / 2.0;
    let size = (half_w * 0.90).min(h * 0.90);

    draw_dial(speed, has_fix, cr, cx_dial, cy_dial, size);

    // Subtle vertical divider between the two halves.
    cr.set_source_rgba(0.25, 0.25, 0.28, 0.5);
    cr.set_line_width(1.0);
    cr.move_to(half_w, h * 0.08);
    cr.line_to(half_w, h * 0.92);
    cr.stroke().ok();

    // Right panel: centred at x = w * 0.75.
    let cx_info = half_w + half_w / 2.0;

    // GPS status near the upper portion of the right panel.
    let dot_cy = h * 0.22;
    draw_gps_status(accuracy, has_fix, elapsed_s, cr, cx_info, dot_cy, size);

    // Altitude + coordinates in the lower portion.
    let info_top = h * 0.58;
    let row_gap  = size * 0.075;
    draw_info_labels(altitude, latitude, longitude, has_fix, cr, cx_info, info_top, row_gap, size);
}

// ── Shared drawing helpers ────────────────────────────────────────────────────

/// Draws the full speedometer dial (arc, ticks, needle, digital readout)
/// centred at (cx, cy) with the given size parameter.
fn draw_dial(
    speed: f64,
    has_fix: bool,
    cr: &gtk::cairo::Context,
    cx: f64,
    cy: f64,
    size: f64,
) {
    let r           = size * 0.42;
    let track_width = size * 0.038;

    let speed_clamped = speed.clamp(0.0, MAX_SPEED);
    let needle_rad = (START_DEG + (speed_clamped / MAX_SPEED) * SWEEP_DEG).to_radians();
    let start_rad  = START_DEG.to_radians();
    let end_rad    = (START_DEG + SWEEP_DEG).to_radians();

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
    let r_label       = r - size * 0.14;

    for v in (0u32..=200).step_by(10) {
        let t_rad  = (START_DEG + (v as f64 / MAX_SPEED) * SWEEP_DEG).to_radians();
        let cos_t  = t_rad.cos();
        let sin_t  = t_rad.sin();
        let is_major = v % 20 == 0;
        let r_inner  = if is_major { r_major_inner } else { r_minor_inner };

        cr.set_source_rgb(0.80, 0.80, 0.82);
        cr.set_line_width(if is_major { size * 0.013 } else { size * 0.007 });
        cr.move_to(cx + r_inner * cos_t, cy + r_inner * sin_t);
        cr.line_to(cx + r       * cos_t, cy + r       * sin_t);
        cr.stroke().ok();
    }

    // ── Speed labels (0, 40, 80, 120, 160, 200) ───────────────────────────
    cr.set_source_rgb(0.88, 0.88, 0.90);
    cr.set_font_size(size * 0.055);
    cr.select_font_face("Sans", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Normal);

    for v in [0u32, 40, 80, 120, 160, 200] {
        let t_rad = (START_DEG + (v as f64 / MAX_SPEED) * SWEEP_DEG).to_radians();
        let lx = cx + r_label * t_rad.cos();
        let ly = cy + r_label * t_rad.sin();
        let label = v.to_string();
        if let Ok(ext) = cr.text_extents(&label) {
            cr.move_to(lx - ext.width() / 2.0 - ext.x_bearing(), ly + ext.height() / 2.0);
            cr.show_text(&label).ok();
        }
    }

    // ── Needle ────────────────────────────────────────────────────────────
    let needle_cos = needle_rad.cos();
    let needle_sin = needle_rad.sin();

    // Shadow for depth.
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.4);
    cr.set_line_width(size * 0.018);
    cr.set_line_cap(gtk::cairo::LineCap::Round);
    cr.move_to(cx - r * 0.18 * needle_cos + 2.0, cy - r * 0.18 * needle_sin + 2.0);
    cr.line_to(cx + r * 0.88 * needle_cos + 2.0, cy + r * 0.88 * needle_sin + 2.0);
    cr.stroke().ok();

    let (nr, ng, nb) = if has_fix { (0.92, 0.22, 0.22) } else { (0.55, 0.55, 0.55) };
    cr.set_source_rgb(nr, ng, nb);
    cr.set_line_width(size * 0.014);
    cr.move_to(cx - r * 0.18 * needle_cos, cy - r * 0.18 * needle_sin);
    cr.line_to(cx + r * 0.88 * needle_cos, cy + r * 0.88 * needle_sin);
    cr.stroke().ok();

    // Centre hub.
    cr.arc(cx, cy, size * 0.028, 0.0, 2.0 * PI);
    cr.set_source_rgb(0.75, 0.75, 0.77);
    cr.fill().ok();
    cr.arc(cx, cy, size * 0.012, 0.0, 2.0 * PI);
    cr.set_source_rgb(0.30, 0.30, 0.32);
    cr.fill().ok();

    // ── Digital speed readout (inside the dial, below centre) ─────────────
    let speed_str = if has_fix { format!("{:.0}", speed_clamped) } else { "--".to_string() };
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.set_font_size(size * 0.16);
    cr.select_font_face("Sans", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Bold);
    let speed_y = cy + size * 0.22;
    if let Ok(ext) = cr.text_extents(&speed_str) {
        cr.move_to(cx - ext.width() / 2.0 - ext.x_bearing(), speed_y);
        cr.show_text(&speed_str).ok();
    }

    // "km/h" label just below the number.
    cr.set_source_rgb(0.55, 0.57, 0.60);
    cr.set_font_size(size * 0.055);
    cr.select_font_face("Sans", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Normal);
    if let Ok(ext) = cr.text_extents("km/h") {
        cr.move_to(cx - ext.width() / 2.0 - ext.x_bearing(), speed_y + size * 0.075);
        cr.show_text("km/h").ok();
    }
}

/// Draws the GPS status dot, accuracy label, and optional hint text,
/// centred horizontally at dot_cx with the dot's centre at dot_cy.
fn draw_gps_status(
    accuracy: f64,
    has_fix: bool,
    elapsed_s: i64,
    cr: &gtk::cairo::Context,
    dot_cx: f64,
    dot_cy: f64,
    size: f64,
) {
    let dot_r    = size * 0.022;
    let line_gap = size * 0.052;

    cr.arc(dot_cx, dot_cy, dot_r, 0.0, 2.0 * PI);
    if !has_fix {
        cr.set_source_rgb(0.90, 0.20, 0.20);
    } else if accuracy < 10.0 {
        cr.set_source_rgb(0.18, 0.85, 0.30);
    } else {
        cr.set_source_rgb(0.95, 0.78, 0.10);
    }
    cr.fill().ok();

    cr.set_source_rgb(0.55, 0.57, 0.60);
    cr.set_font_size(size * 0.042);
    cr.select_font_face("Sans", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Normal);
    let gps_label = if has_fix {
        if accuracy >= 1000.0 {
            format!("±{:.1} km", accuracy / 1000.0)
        } else {
            format!("±{:.0} m", accuracy)
        }
    } else {
        "No signal".to_string()
    };
    let label_y = dot_cy + dot_r + line_gap;
    if let Ok(ext) = cr.text_extents(&gps_label) {
        cr.move_to(dot_cx - ext.width() / 2.0 - ext.x_bearing(), label_y);
        cr.show_text(&gps_label).ok();
    }

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
            cr.move_to(dot_cx - ext.width() / 2.0 - ext.x_bearing(), label_y + line_gap);
            cr.show_text(hint_str).ok();
        }
    }

    // Time-to-fix line: counts up while searching, freezes when fix is acquired.
    let timer_str = if !has_fix {
        format!("Searching… {}s", elapsed_s)
    } else if elapsed_s >= 0 {
        format!("Fixed in {}s", elapsed_s)
    } else {
        return;
    };
    let timer_y = label_y + line_gap * (if hint.is_some() { 2.0 } else { 1.0 });
    cr.set_font_size(size * 0.034);
    cr.set_source_rgb(0.38, 0.40, 0.44);
    if let Ok(ext) = cr.text_extents(&timer_str) {
        cr.move_to(dot_cx - ext.width() / 2.0 - ext.x_bearing(), timer_y);
        cr.show_text(&timer_str).ok();
    }
}

/// Draws the altitude and coordinate rows, starting at panel_top,
/// horizontally centred at cx.  No-ops when has_fix is false.
fn draw_info_labels(
    altitude: f64,
    latitude: f64,
    longitude: f64,
    has_fix: bool,
    cr: &gtk::cairo::Context,
    cx: f64,
    panel_top: f64,
    row_gap: f64,
    size: f64,
) {
    if !has_fix {
        return;
    }

    cr.select_font_face("Sans", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Normal);

    // ── Altitude ──────────────────────────────────────────────────────────
    let alt_str = format!("Altitude: {:.0} m", altitude);
    cr.set_font_size(size * 0.052);
    cr.set_source_rgb(0.62, 0.64, 0.68);
    if let Ok(ext) = cr.text_extents(&alt_str) {
        cr.move_to(cx - ext.width() / 2.0 - ext.x_bearing(), panel_top);
        cr.show_text(&alt_str).ok();
    }

    // ── Latitude ──────────────────────────────────────────────────────────
    let lat_hem = if latitude >= 0.0 { "N" } else { "S" };
    let lat_str = format!("Lat: {:.5}° {}", latitude.abs(), lat_hem);
    cr.set_font_size(size * 0.046);
    cr.set_source_rgb(0.50, 0.52, 0.56);
    if let Ok(ext) = cr.text_extents(&lat_str) {
        cr.move_to(cx - ext.width() / 2.0 - ext.x_bearing(), panel_top + row_gap);
        cr.show_text(&lat_str).ok();
    }

    // ── Longitude ─────────────────────────────────────────────────────────
    let lon_hem = if longitude >= 0.0 { "E" } else { "W" };
    let lon_str = format!("Lon: {:.5}° {}", longitude.abs(), lon_hem);
    cr.set_font_size(size * 0.046);
    cr.set_source_rgb(0.50, 0.52, 0.56);
    if let Ok(ext) = cr.text_extents(&lon_str) {
        cr.move_to(cx - ext.width() / 2.0 - ext.x_bearing(), panel_top + row_gap * 2.0);
        cr.show_text(&lon_str).ok();
    }
}

