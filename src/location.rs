/* location.rs
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

use ashpd::desktop::location::{Accuracy, CreateSessionOptions, LocationProxy};
use async_channel::Sender;
use futures_util::StreamExt;

/// Data sent from the GPS background thread to the UI thread.
pub struct LocationData {
    /// Current speed in km/h (0.0 when no fix or speed unavailable).
    pub speed_kmh: f64,
    /// Altitude in metres above sea level (0.0 when unavailable).
    pub altitude_m: f64,
    /// True once the portal has delivered at least one valid location fix.
    pub has_fix: bool,
}

/// Spawn a background thread running a Tokio current-thread runtime that
/// subscribes to the XDG Location Portal and forwards updates via `sender`.
pub fn start_location_watching(sender: Sender<LocationData>) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to build Tokio runtime for location");

        rt.block_on(async {
            if let Err(e) = watch_location(sender).await {
                eprintln!("Location portal error: {e}");
            }
        });
    });
}

async fn watch_location(sender: Sender<LocationData>) -> ashpd::Result<()> {
    let proxy = LocationProxy::new().await?;

    let session = proxy
        .create_session(
            CreateSessionOptions::default()
                .set_accuracy(Accuracy::Exact)
                .set_distance_threshold(0u32)
                .set_time_threshold(1u32),
        )
        .await?;

    // Subscribe to the signal stream *before* calling start so we don't miss
    // the first update.
    let mut stream = proxy.receive_location_updated().await?;

    // start() returns as soon as the portal has acknowledged the request; the
    // actual location arrives on the stream.
    proxy.start(&session, None, Default::default()).await?;

    while let Some(location) = stream.next().await {
        // speed() returns None when the portal reports -1 (unavailable).
        let speed_kmh = location.speed().unwrap_or(0.0).max(0.0) * 3.6;
        // altitude() returns None when the portal reports the sentinel value.
        let altitude_m = location.altitude().unwrap_or(0.0);

        sender.send_blocking(LocationData {
            speed_kmh,
            altitude_m,
            has_fix: true,
        }).ok();
    }

    Ok(())
}
