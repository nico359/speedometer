/* accelerometer.rs
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

use async_channel::Sender;
use futures_util::StreamExt;
use zbus::{Connection, MatchRule, MessageStream};

/// Processed accelerometer update forwarded to the UI thread.
pub struct AccelData {
    /// Sensor timestamp in microseconds (from sensorfw).
    pub timestamp_us: u64,
    /// Magnitude of linear acceleration in m/s² after gravity removal (≥ 0).
    pub linear_ms2: f64,
}

/// Spawn a background thread that subscribes to the sensorfw accelerometer
/// over D-Bus and forwards processed updates via `sender`.
pub fn start_accelerometer_watching(sender: Sender<AccelData>) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to build Tokio runtime for accelerometer");

        rt.block_on(async {
            loop {
                if let Err(e) = watch_accelerometer(&sender).await {
                    eprintln!("Accelerometer watcher error: {e}");
                }
                // Brief pause before retrying to avoid tight error loops.
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        });
    });
}

async fn watch_accelerometer(sender: &Sender<AccelData>) -> zbus::Result<()> {
    let conn = Connection::system().await?;
    let pid = std::process::id() as i64;

    // Request a sensorfw session for the accelerometer.
    let session_id: i32 = conn
        .call_method(
            Some("com.nokia.SensorService"),
            "/SensorManager",
            Some("local.SensorManager"),
            "requestSensor",
            &("accelerometersensor", pid),
        )
        .await?
        .body()
        .deserialize::<(i32,)>()?
        .0;

    // Activate the sensor for our session so sensorfw starts the signal stream.
    conn.call_method(
        Some("com.nokia.SensorService"),
        "/SensorManager/accelerometersensor",
        Some("local.AccelerometerSensor"),
        "start",
        &(session_id,),
    )
    .await?;

    // Subscribe to the dataAvailable signal.
    // Body signature: (tddd) — timestamp_µs (u64), x, y, z in mG (f64 each).
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("com.nokia.SensorService")?
        .path("/SensorManager/accelerometersensor")?
        .interface("local.AccelerometerSensor")?
        .member("dataAvailable")?
        .build();

    let mut stream = MessageStream::for_match_rule(rule, &conn, Some(64)).await?;

    // Low-pass filter to track gravity and isolate linear acceleration.
    // α ≈ 0.926 gives a ~1 s time constant at the default 80 ms sample interval.
    const ALPHA: f64 = 0.926;
    const MG_TO_MS2: f64 = 9.81 / 1000.0;
    let mut gravity: Option<(f64, f64, f64)> = None;

    while let Some(msg) = stream.next().await {
        let msg = msg?;
        let (ts, x_mg, y_mg, z_mg): (u64, f64, f64, f64) = msg.body().deserialize()?;

        let (x, y, z) = (x_mg * MG_TO_MS2, y_mg * MG_TO_MS2, z_mg * MG_TO_MS2);

        let g = match &mut gravity {
            None => {
                // Initialise gravity estimate with the first reading.
                gravity = Some((x, y, z));
                continue;
            }
            Some(g) => {
                g.0 = ALPHA * g.0 + (1.0 - ALPHA) * x;
                g.1 = ALPHA * g.1 + (1.0 - ALPHA) * y;
                g.2 = ALPHA * g.2 + (1.0 - ALPHA) * z;
                *g
            }
        };

        // Linear acceleration = total − gravity.
        let (lx, ly, lz) = (x - g.0, y - g.1, z - g.2);
        let linear_ms2 = (lx * lx + ly * ly + lz * lz).sqrt();

        sender.send_blocking(AccelData { timestamp_us: ts, linear_ms2 }).ok();
    }

    Ok(())
}
