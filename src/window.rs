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
use glib::prelude::ToVariant;

// ── Drawing constants ────────────────────────────────────────────────────────

/// Cairo angle (radians) at which the 0 km/h position sits.
/// Cairo angles: 0 = right, increase clockwise on screen.
/// 150° places the start at the lower-left (~8 o'clock).
const START_DEG: f64 = 150.0;

/// Total angular sweep of the dial (240° covers 8-o'clock → top → 4-o'clock).
const SWEEP_DEG: f64 = 240.0;

/// Spring stiffness: controls how fast the needle accelerates toward the target.
/// ω₀ = sqrt(SPRING_K) ≈ 8.9 rad/s → natural period ≈ 0.7 s.
const SPRING_K: f64 = 80.0;

/// Spring damping: 2·√K·0.75 ≈ slightly underdamped — fast response with a
/// barely-perceptible settle, like a real analogue pointer.
const SPRING_D: f64 = 13.4;

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
        pub needle_velocity: Cell<f64>,  // spring velocity (km/h per second)

        // Time-to-fix tracking
        pub search_start_us: Cell<i64>,  // glib::monotonic_time() when search began
        pub time_to_fix_s:   Cell<i64>,  // seconds until fix was acquired; -1 = not yet
        pub prev_has_fix:    Cell<bool>, // last known fix state for transition detection

        // Unit preference
        pub use_mph: Cell<bool>,

        // IMU-assisted speed fusion.
        // Between GPS fixes the accel integrates Δv; GPS re-anchors on every fix.
        // Gyro-based corner suppression prevents centripetal force from
        // being wrongly integrated as braking during turns.
        pub accel_base_speed: Cell<f64>,  // GPS speed at last fix (km/h)
        pub accel_delta:      Cell<f64>,  // integrated Δv since last GPS fix (km/h)
        pub accel_sign:       Cell<f64>,  // +1.0 accel, −1.0 decel, 0.0 unknown
        pub accel_last_ts:    Cell<u64>,  // timestamp of most recent accel sample (µs)
        pub accel_has_gps:    Cell<bool>, // true once GPS has delivered a valid fix
        pub gps_prev_speed:   Cell<f64>,  // speed from previous GPS update (km/h)
        pub gps_prev_heading: Cell<f64>,  // GPS COG from previous fix (degrees); NAN = unknown
        pub accel_enabled:    Cell<bool>, // whether IMU assist is active (user toggle)

        // G-force display state.
        // Raw values are updated on every IMU sample regardless of the fusion
        // toggle, so the G-meter always shows live sensor data.
        // Smoothed values are advanced each frame by the 60 fps ticker.
        pub gforce_x_raw: Cell<f64>, // lateral G  (accel_x / 9.81), sensor frame
        pub gforce_y_raw: Cell<f64>, // longitudinal G (accel_y / 9.81), sensor frame
        pub gforce_x:     Cell<f64>, // EMA-smoothed lateral G for display
        pub gforce_y:     Cell<f64>, // EMA-smoothed longitudinal G for display
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

            // Stateful action for speed unit (kmh / mph).
            let action_unit = gio::SimpleAction::new_stateful(
                "speed-unit",
                Some(glib::VariantTy::STRING),
                &"kmh".to_variant(),
            );
            let win_weak_u = obj.downgrade();
            action_unit.connect_activate(move |action, param| {
                if let Some(win) = win_weak_u.upgrade() {
                    let unit = param.and_then(|v| v.str()).unwrap_or("kmh");
                    action.set_state(&unit.to_variant());
                    win.imp().use_mph.set(unit == "mph");
                }
            });
            obj.add_action(&action_unit);

            // Stateful toggle action for accelerometer assist (default: on).
            self.accel_enabled.set(true);
            let action_accel = gio::SimpleAction::new_stateful(
                "accel-enabled",
                None,
                &true.to_variant(),
            );
            let win_weak_ac = obj.downgrade();
            action_accel.connect_activate(move |action, _| {
                if let Some(win) = win_weak_ac.upgrade() {
                    let current = action.state()
                        .and_then(|v| v.get::<bool>())
                        .unwrap_or(true);
                    let next = !current;
                    action.set_state(&next.to_variant());
                    let imp = win.imp();
                    imp.accel_enabled.set(next);
                    // Reset fusion state when disabling so stale delta doesn't linger.
                    if !next {
                        imp.accel_delta.set(0.0);
                    }
                }
            });
            obj.add_action(&action_accel);

            // Start the search timer immediately on construction.
            self.search_start_us.set(glib::monotonic_time());
            self.time_to_fix_s.set(-1);
            // Heading unknown until first GPS fix with COG data.
            self.gps_prev_heading.set(f64::NAN);

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
                        imp.use_mph.get(),
                        imp.gforce_x.get(),
                        imp.gforce_y.get(),
                        imp.accel_enabled.get(),
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

                        // Update IMU fusion anchor on every GPS fix.
                        // Use GPS COG (Course Over Ground) to check if heading
                        // changed significantly between fixes.  COG is purely
                        // Doppler-derived — no magnetometer/compass involved.
                        // If we turned ≥ 15° the speed delta is unreliable for
                        // sign detection (centripetal effect), so we keep the
                        // current sign rather than flipping it wrongly.
                        let prev_gps = imp.gps_prev_speed.get();
                        let prev_hdg = imp.gps_prev_heading.get();
                        let cur_hdg  = data.heading_deg.unwrap_or(f64::NAN);

                        let heading_stable = if prev_hdg.is_nan() || cur_hdg.is_nan() {
                            // No heading data — fall back to old behaviour.
                            true
                        } else {
                            // Shortest angular distance between two bearings.
                            let diff = ((cur_hdg - prev_hdg + 540.0) % 360.0) - 180.0;
                            diff.abs() < 15.0
                        };

                        let sign = if heading_stable && data.speed_kmh - prev_gps > 0.5 {
                            1.0   // accelerating on a straight road
                        } else if heading_stable && prev_gps - data.speed_kmh > 0.5 {
                            -1.0  // braking on a straight road
                        } else {
                            imp.accel_sign.get() // turning or cruising — keep current sign
                        };
                        imp.accel_sign.set(sign);
                        imp.gps_prev_speed.set(data.speed_kmh);
                        imp.gps_prev_heading.set(cur_hdg);
                        imp.accel_base_speed.set(data.speed_kmh);
                        imp.accel_delta.set(0.0);
                        imp.accel_has_gps.set(good_fix);

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

            // IMU channel: fills in speed between GPS fixes with gyro-aided
            // corner suppression.
            let (imu_sender, imu_receiver) =
                async_channel::bounded::<crate::imu::ImuData>(4);

            crate::imu::start_imu_watching(imu_sender);

            let win_weak_a = obj.downgrade();
            glib::MainContext::default().spawn_local(async move {
                while let Ok(data) = imu_receiver.recv().await {
                    if let Some(win) = win_weak_a.upgrade() {
                        let imp = win.imp();

                    // Always update G-force display regardless of GPS or toggle state.
                    // Phone is mounted upright: X = lateral, Z = longitudinal (forward/back).
                    const G: f64 = 9.81;
                    imp.gforce_x_raw.set(data.accel_x_ms2 / G);
                    imp.gforce_y_raw.set(-data.accel_z_ms2 / G); // negated: forward accel = positive G

                    // Fusion requires a GPS fix and the IMU assist toggle to be enabled.
                    if !imp.accel_has_gps.get() || !imp.accel_enabled.get() {
                            imp.accel_last_ts.set(data.timestamp_us);
                            continue;
                        }

                        let last_ts = imp.accel_last_ts.get();
                        imp.accel_last_ts.set(data.timestamp_us);

                        if last_ts == 0 { continue; }

                        let dt_s = data.timestamp_us.saturating_sub(last_ts) as f64 / 1_000_000.0;
                        if dt_s <= 0.0 || dt_s > 0.5 { continue; }

                        let sign = imp.accel_sign.get();
                        if sign == 0.0 { continue; }

                        // Use only the longitudinal component (forward/back axis) for
                        // fusion. Phone is mounted upright in the car, so the Z axis
                        // (into/out of screen) is forward/back; Y is vertical (gravity axis)
                        // and X is lateral. Bumps are mostly on Y (gravity-removed ≈ small),
                        // cornering is on X (suppressed by gyro threshold).
                        let fwd_ms2 = data.accel_z_ms2.abs();

                        // Threshold: ignore anything below 0.8 m/s² on the forward axis.
                        // This filters road vibration, gentle curves and sensor noise.
                        // Only meaningful acceleration/braking events pass through.
                        const THRESHOLD: f64 = 0.8;

                        if fwd_ms2 < THRESHOLD {
                            // Below threshold: decay delta back toward 0 so stale
                            // negative corrections don't keep the speed artificially low.
                            let decay = imp.accel_delta.get() * 0.92_f64.powf(dt_s / 0.08);
                            imp.accel_delta.set(decay);
                            continue;
                        }

                        // Gyro-based corner suppression.
                        // Below GYRO_LO rad/s: straight line → full accel weight.
                        // Above GYRO_HI rad/s: clear turn → suppress entirely.
                        const GYRO_LO: f64 = 0.10;
                        const GYRO_HI: f64 = 0.30;
                        let weight = 1.0 - ((data.gyro_rads - GYRO_LO) / (GYRO_HI - GYRO_LO))
                            .clamp(0.0, 1.0);

                        if weight < 0.01 { continue; }

                        // Convert: m/s² × s × weight × 3.6 → km/h change
                        let delta_kmh = fwd_ms2 * dt_s * sign * weight * 3.6;
                        let new_delta = (imp.accel_delta.get() + delta_kmh).clamp(-20.0, 20.0);
                        imp.accel_delta.set(new_delta);

                        let fused = (imp.accel_base_speed.get() + new_delta).max(0.0);
                        set_speed_target(imp, fused);
                    }
                }
            });

            // 60 fps ticker: advance the EMA and redraw.
            let win_weak3 = obj.downgrade();
            glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
                if let Some(win) = win_weak3.upgrade() {
                    let imp = win.imp();
                    // Spring-damped needle: acceleration = K·error − D·velocity.
                    // Slightly underdamped (ratio ≈ 0.75) so the needle swings in
                    // fast and settles with a barely-perceptible analogue overshoot.
                    const DT: f64 = 0.016;
                    let displayed = imp.speed_displayed.get();
                    let target    = imp.speed_target.get();
                    let vel       = imp.needle_velocity.get();
                    let accel     = SPRING_K * (target - displayed) - SPRING_D * vel;
                    let new_vel   = vel + accel * DT;
                    imp.needle_velocity.set(new_vel);
                    imp.speed_displayed.set((displayed + new_vel * DT).max(0.0));

                    // G-force display smoothing (independent of speed fusion toggle).
                    const GFORCE_ALPHA: f64 = 0.5;
                    let gx_disp = imp.gforce_x.get();
                    let gy_disp = imp.gforce_y.get();
                    imp.gforce_x.set(gx_disp + (imp.gforce_x_raw.get() - gx_disp) * GFORCE_ALPHA);
                    imp.gforce_y.set(gy_disp + (imp.gforce_y_raw.get() - gy_disp) * GFORCE_ALPHA);
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
    use_mph: bool,
    gforce_x: f64,
    gforce_y: f64,
    accel_enabled: bool,
    cr: &gtk::cairo::Context,
    width: i32,
    height: i32,
) {
    let w = width as f64;
    let h = height as f64;

    cr.set_source_rgb(0.10, 0.10, 0.12);
    cr.paint().ok();

    if w > h {
        draw_landscape(speed, altitude, accuracy, has_fix, latitude, longitude, elapsed_s, use_mph, gforce_x, gforce_y, accel_enabled, cr, w, h);
    } else {
        draw_portrait(speed, altitude, accuracy, has_fix, latitude, longitude, elapsed_s, use_mph, gforce_x, gforce_y, accel_enabled, cr, w, h);
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
    use_mph: bool,
    gforce_x: f64,
    gforce_y: f64,
    accel_enabled: bool,
    cr: &gtk::cairo::Context,
    w: f64,
    h: f64,
) {
    // top_margin is sized generously so the GPS block (dot + two text lines)
    // never overlaps the dial even on near-square windows.
    let top_margin = h * 0.17;
    let bot_margin = h * 0.22;
    let usable_h   = h - top_margin - bot_margin;

    // size drives every dial dimension; cap it so the dial never overflows
    // into either margin, regardless of how wide the window gets.
    let size = w.min(usable_h / 0.84);   // 0.84 ≈ dial diameter / size

    let cx = w / 2.0;
    // Centre the dial in the usable vertical band.
    let cy = top_margin + usable_h / 2.0;

    draw_dial(speed, has_fix, use_mph, cr, cx, cy, size);

    // GPS status indicator sits in the top margin band, anchored to top_margin
    // so it scales correctly instead of being fixed at h*0.05.
    let dot_cy = top_margin * 0.38;
    draw_gps_status(accuracy, has_fix, elapsed_s, cr, cx, dot_cy, size);

    // Info panel starts just below the dial background circle.
    let r           = size * 0.42;
    let track_width = size * 0.038;
    let panel_top = cy + r + track_width * 1.2 + size * 0.06;
    let row_gap   = size * 0.075;

    let band_center_y = (panel_top + h) / 2.0;
    let available_h   = h - panel_top;

    if accel_enabled {
        // Split the info band vertically at ~40% from the left.
        // G-force meter on the left, coordinate text on the right.
        let gf_radius = (available_h * 0.32).min(w * 0.18);
        let gf_cx     = w * 0.22;
        draw_gforce_meter(gforce_x, gforce_y, cr, gf_cx, band_center_y, gf_radius);

        // Text: 3 rows spanning 2×row_gap, centred at band_center_y.
        let text_cx  = gf_cx + gf_radius + (w - gf_cx - gf_radius) / 2.0;
        let text_top = band_center_y - row_gap;
        draw_info_labels(altitude, latitude, longitude, has_fix, cr, text_cx, text_top, row_gap, size);
    } else {
        // Accel disabled: full-width info labels + small warning just below.
        let text_top = band_center_y - row_gap * 2.0;
        draw_info_labels(altitude, latitude, longitude, has_fix, cr, cx, text_top, row_gap, size);
        // Note: place below last info row with a fixed offset, capped so it never
        // bleeds off the bottom edge.
        let note_cy = (text_top + 3.5 * row_gap).min(h - size * 0.09);
        draw_accel_disabled_note(cr, cx, note_cy, size);
    }
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
    use_mph: bool,
    gforce_x: f64,
    gforce_y: f64,
    accel_enabled: bool,
    cr: &gtk::cairo::Context,
    w: f64,
    h: f64,
) {
    let half_w = w / 2.0;

    // Dial fills the left half, vertically centred.
    let cx_dial = half_w / 2.0;
    let cy_dial = h / 2.0;
    let size = (half_w * 0.90).min(h * 0.90);

    draw_dial(speed, has_fix, use_mph, cr, cx_dial, cy_dial, size);

    // Subtle vertical divider between the two halves.
    cr.set_source_rgba(0.25, 0.25, 0.28, 0.5);
    cr.set_line_width(1.0);
    cr.move_to(half_w, h * 0.08);
    cr.line_to(half_w, h * 0.92);
    cr.stroke().ok();

    if accel_enabled {
        // Right panel: split left/right so G-meter and text don't overlap.
        // Left quarter of the right half → G-force meter.
        // Right quarter of the right half → GPS status + coordinates.
        let cx_gf   = half_w + half_w * 0.28;
        let cx_info = half_w + half_w * 0.72;

        let dot_cy = h * 0.18;
        draw_gps_status(accuracy, has_fix, elapsed_s, cr, cx_info, dot_cy, size);

        let gf_radius = (h * 0.30).min(half_w * 0.26);
        let gf_cy = h * 0.50;
        draw_gforce_meter(gforce_x, gforce_y, cr, cx_gf, gf_cy, gf_radius);

        let info_top = h * 0.60;
        let row_gap  = size * 0.075;
        draw_info_labels(altitude, latitude, longitude, has_fix, cr, cx_info, info_top, row_gap, size);
    } else {
        // Accel disabled: single info column centred in the right half.
        let cx_info = half_w + half_w / 2.0;
        let dot_cy  = h * 0.15;
        draw_gps_status(accuracy, has_fix, elapsed_s, cr, cx_info, dot_cy, size);

        let row_gap  = size * 0.075;
        let info_top = h * 0.40;
        draw_info_labels(altitude, latitude, longitude, has_fix, cr, cx_info, info_top, row_gap, size);
        let note_cy = (info_top + 3.5 * row_gap).min(h * 0.90);
        draw_accel_disabled_note(cr, cx_info, note_cy, size);
    }
}

// ── Shared drawing helpers ────────────────────────────────────────────────────

/// Draws the full speedometer dial (arc, ticks, needle, digital readout)
/// centred at (cx, cy) with the given size parameter.
fn draw_dial(
    speed: f64,
    has_fix: bool,
    use_mph: bool,
    cr: &gtk::cairo::Context,
    cx: f64,
    cy: f64,
    size: f64,
) {
    let r           = size * 0.42;
    let track_width = size * 0.038;

    // Unit-specific scale: km/h uses 0–200, mph uses 0–120.
    let (max_speed, display_speed, tick_majors, tick_minor_step, unit_label): (f64, f64, &[u32], u32, &str) =
        if use_mph {
            (120.0, speed * 0.621_371, &[0, 20, 40, 60, 80, 100, 120], 10, "mph")
        } else {
            (200.0, speed, &[0, 40, 80, 120, 160, 200], 10, "km/h")
        };

    let speed_clamped = display_speed.clamp(0.0, max_speed);
    let needle_rad = (START_DEG + (speed_clamped / max_speed) * SWEEP_DEG).to_radians();
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
        let ratio = speed_clamped / max_speed;
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

    // Minor ticks at every tick_minor_step across the full scale.
    let tick_count = (max_speed / tick_minor_step as f64) as u32;
    for i in 0..=tick_count {
        let v = i * tick_minor_step;
        let t_rad  = (START_DEG + (v as f64 / max_speed) * SWEEP_DEG).to_radians();
        let cos_t  = t_rad.cos();
        let sin_t  = t_rad.sin();
        let is_major = tick_majors.contains(&v);
        let r_inner  = if is_major { r_major_inner } else { r_minor_inner };

        cr.set_source_rgb(0.80, 0.80, 0.82);
        cr.set_line_width(if is_major { size * 0.013 } else { size * 0.007 });
        cr.move_to(cx + r_inner * cos_t, cy + r_inner * sin_t);
        cr.line_to(cx + r       * cos_t, cy + r       * sin_t);
        cr.stroke().ok();
    }

    // ── Speed labels at major tick positions ──────────────────────────────
    cr.set_source_rgb(0.88, 0.88, 0.90);
    cr.set_font_size(size * 0.055);
    cr.select_font_face("Sans", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Normal);

    for &v in tick_majors {
        let t_rad = (START_DEG + (v as f64 / max_speed) * SWEEP_DEG).to_radians();
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
    // Show true speed even when needle is pinned at max.
    let speed_str = if has_fix { format!("{:.0}", display_speed.max(0.0)) } else { "--".to_string() };
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.set_font_size(size * 0.16);
    cr.select_font_face("Sans", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Bold);
    let speed_y = cy + size * 0.22;
    if let Ok(ext) = cr.text_extents(&speed_str) {
        cr.move_to(cx - ext.width() / 2.0 - ext.x_bearing(), speed_y);
        cr.show_text(&speed_str).ok();
    }

    // Unit label just below the number.
    cr.set_source_rgb(0.55, 0.57, 0.60);
    cr.set_font_size(size * 0.055);
    cr.select_font_face("Sans", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Normal);
    if let Ok(ext) = cr.text_extents(unit_label) {
        cr.move_to(cx - ext.width() / 2.0 - ext.x_bearing(), speed_y + size * 0.075);
        cr.show_text(unit_label).ok();
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

// ── G-force meter ─────────────────────────────────────────────────────────────

/// Draws a 2-D bullseye G-force meter centred at (cx, cy).
///
/// `gx` is the lateral G-force (sensor X / 9.81) — typically left/right.
/// `gy` is the longitudinal G-force (sensor Y / 9.81) — typically forward/back.
/// `radius` is the display radius that corresponds to 1 G.
///
/// The dot colour transitions green → yellow → red with total G.
/// A small status label (IDLE / ACC / BRAKE / TURN L / TURN R) is shown above the
/// meter so the user can verify which axis does what while stationary/moving.
///
/// Note on sign conventions: the sensor axes depend on phone mounting orientation.
/// The display is intentionally unlabelled so the user can verify empirically
/// which direction moves the dot.
/// Small dimmed note shown in place of the G-force meter when accel assist is off.
fn draw_accel_disabled_note(cr: &gtk::cairo::Context, cx: f64, cy: f64, size: f64) {
    cr.select_font_face("Sans", gtk::cairo::FontSlant::Italic, gtk::cairo::FontWeight::Normal);
    let font_size = size * 0.038;
    cr.set_font_size(font_size);
    cr.set_source_rgba(0.55, 0.56, 0.60, 0.90);

    let lines = [
        "Accel assist off",
        "Speed may feel less responsive",
        "during rapid acceleration",
    ];
    let line_h = font_size * 1.5;
    let total_h = line_h * (lines.len() as f64 - 1.0);
    let mut y = cy - total_h / 2.0;
    for line in &lines {
        if let Ok(ext) = cr.text_extents(line) {
            cr.move_to(cx - ext.width() / 2.0 - ext.x_bearing(), y);
            cr.show_text(line).ok();
        }
        y += line_h;
    }
}

fn draw_gforce_meter(
    gx: f64,
    gy: f64,
    cr: &gtk::cairo::Context,
    cx: f64,
    cy: f64,
    radius: f64,
) {
    let r = radius;
    const THRESHOLD: f64 = 0.08; // G below which an axis is considered idle

    // ── Background circle ──────────────────────────────────────────────────
    cr.arc(cx, cy, r * 1.10, 0.0, 2.0 * PI);
    cr.set_source_rgb(0.07, 0.07, 0.09);
    cr.fill().ok();

    // ── Concentric rings at 0.25 G, 0.5 G, 1.0 G ──────────────────────────
    for (frac, alpha, lw) in &[
        (1.00_f64, 0.65_f64, 0.040_f64),
        (0.50_f64, 0.40_f64, 0.025_f64),
        (0.25_f64, 0.28_f64, 0.018_f64),
    ] {
        cr.arc(cx, cy, r * frac, 0.0, 2.0 * PI);
        cr.set_source_rgba(0.35, 0.35, 0.38, *alpha);
        cr.set_line_width(r * lw);
        cr.stroke().ok();
    }

    // ── Crosshairs ─────────────────────────────────────────────────────────
    cr.set_source_rgba(0.32, 0.32, 0.35, 0.55);
    cr.set_line_width(r * 0.018);
    cr.move_to(cx - r, cy); cr.line_to(cx + r, cy); cr.stroke().ok();
    cr.move_to(cx, cy - r); cr.line_to(cx, cy + r); cr.stroke().ok();

    // ── Dot ────────────────────────────────────────────────────────────────
    let total_g = (gx * gx + gy * gy).sqrt();
    let dot_x = cx + gx.clamp(-1.0, 1.0) * r;
    let dot_y = cy - gy.clamp(-1.0, 1.0) * r;  // screen Y inverted

    // Colour: green (low) → yellow (medium) → red (high), threshold at 0.5 G.
    let (red, green) = if total_g <= 0.5 {
        (total_g * 2.0, 1.0)
    } else {
        (1.0, 1.0 - (total_g - 0.5) * 2.0)
    };
    let green = green.clamp(0.0, 1.0);

    cr.arc(dot_x, dot_y, r * 0.14, 0.0, 2.0 * PI);
    cr.set_source_rgb(red, green, 0.05);
    cr.fill().ok();

    // Subtle glow ring around the dot.
    cr.arc(dot_x, dot_y, r * 0.20, 0.0, 2.0 * PI);
    cr.set_source_rgba(red, green, 0.05, 0.25);
    cr.set_line_width(r * 0.05);
    cr.stroke().ok();

    // ── Status label ───────────────────────────────────────────────────────
    let status = if total_g < THRESHOLD {
        "IDLE"
    } else if gy.abs() >= gx.abs() {
        if gy > 0.0 { "ACC" } else { "BRAKE" }
    } else {
        if gx > 0.0 { "TURN R" } else { "TURN L" }
    };

    cr.select_font_face("Sans", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Bold);
    cr.set_font_size(r * 0.30);
    cr.set_source_rgb(0.52, 0.54, 0.58);
    if let Ok(ext) = cr.text_extents(status) {
        cr.move_to(cx - ext.width() / 2.0 - ext.x_bearing(), cy - r * 1.30);
        cr.show_text(status).ok();
    }

    // ── Total-G readout ────────────────────────────────────────────────────
    let g_str = format!("{:.2}g", total_g);
    cr.select_font_face("Sans", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Normal);
    cr.set_font_size(r * 0.28);
    cr.set_source_rgb(0.45, 0.47, 0.50);
    if let Ok(ext) = cr.text_extents(&g_str) {
        cr.move_to(cx - ext.width() / 2.0 - ext.x_bearing(), cy + r * 1.38);
        cr.show_text(&g_str).ok();
    }
}
