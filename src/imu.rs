/* imu.rs
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

/// Combined IMU update forwarded to the UI thread on every accelerometer sample.
pub struct ImuData {
    /// Sensor timestamp in microseconds (from sensorfw accelerometer).
    pub timestamp_us: u64,
    /// Magnitude of linear acceleration in m/s² after gravity removal (≥ 0).
    pub linear_ms2: f64,
    /// Total angular velocity in rad/s from the most recent gyroscope sample.
    /// Used to suppress accel integration during cornering.
    pub gyro_rads: f64,
}

/// Spawn a background thread that subscribes to both the sensorfw accelerometer
/// and gyroscope over D-Bus, then forwards combined updates via `sender`.
pub fn start_imu_watching(sender: Sender<ImuData>) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to build Tokio runtime for IMU");

        rt.block_on(async {
            loop {
                if let Err(e) = watch_imu(&sender).await {
                    eprintln!("IMU watcher error: {e}");
                }
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        });
    });
}

async fn request_sensor(conn: &Connection, name: &str) -> zbus::Result<i32> {
    let pid = std::process::id() as i64;
    let session_id: i32 = conn
        .call_method(
            Some("com.nokia.SensorService"),
            "/SensorManager",
            Some("local.SensorManager"),
            "requestSensor",
            &(name, pid),
        )
        .await?
        .body()
        .deserialize::<(i32,)>()?
        .0;
    Ok(session_id)
}

async fn start_sensor(conn: &Connection, path: &str, interface: &str, session_id: i32) -> zbus::Result<()> {
    conn.call_method(
        Some("com.nokia.SensorService"),
        path,
        Some(interface),
        "start",
        &(session_id,),
    )
    .await?;
    Ok(())
}

async fn watch_imu(sender: &Sender<ImuData>) -> zbus::Result<()> {
    let conn = Connection::system().await?;

    // --- Accelerometer ---
    let accel_session = request_sensor(&conn, "accelerometersensor").await?;
    start_sensor(&conn, "/SensorManager/accelerometersensor", "local.AccelerometerSensor", accel_session).await?;

    let accel_rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("com.nokia.SensorService")?
        .path("/SensorManager/accelerometersensor")?
        .interface("local.AccelerometerSensor")?
        .member("dataAvailable")?
        .build();
    let mut accel_stream = MessageStream::for_match_rule(accel_rule, &conn, Some(64)).await?;

    // --- Gyroscope ---
    // Falls back gracefully: if gyro is unavailable, gyro_rads stays 0.0 and
    // the accel integration runs without suppression (same as before).
    let gyro_available = async {
        let sid = request_sensor(&conn, "gyroscopesensor").await?;
        start_sensor(&conn, "/SensorManager/gyroscopesensor", "local.GyroscopeSensor", sid).await?;
        zbus::Result::Ok(())
    }.await.is_ok();

    let gyro_rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("com.nokia.SensorService")?
        .path("/SensorManager/gyroscopesensor")?
        .interface("local.GyroscopeSensor")?
        .member("dataAvailable")?
        .build();
    // Always create the stream; if gyro isn't running it simply never fires.
    let mut gyro_stream = MessageStream::for_match_rule(gyro_rule, &conn, Some(64)).await?;

    if !gyro_available {
        eprintln!("IMU: gyroscopesensor unavailable, corner suppression disabled");
    }

    // Low-pass filter state for gravity estimation.
    // α ≈ 0.926 → ~1 s time constant at the 80 ms default accel interval.
    const ALPHA: f64 = 0.926;
    const MG_TO_MS2: f64 = 9.81 / 1000.0;
    // Gyro is in mdps (milli-degrees/second); convert to rad/s.
    const MDPS_TO_RADS: f64 = std::f64::consts::PI / 180_000.0;

    let mut gravity: Option<(f64, f64, f64)> = None;
    let mut gyro_rads: f64 = 0.0; // latest total angular velocity

    loop {
        tokio::select! {
            // Gyro sample: update the shared angular-velocity state.
            Some(msg) = gyro_stream.next() => {
                if let Ok(msg) = msg {
                    if let Ok((_, gx, gy, gz)) = msg.body().deserialize::<(u64, f64, f64, f64)>() {
                        let rx = gx * MDPS_TO_RADS;
                        let ry = gy * MDPS_TO_RADS;
                        let rz = gz * MDPS_TO_RADS;
                        gyro_rads = (rx * rx + ry * ry + rz * rz).sqrt();
                    }
                }
            }

            // Accel sample: compute linear acceleration and emit ImuData.
            Some(msg) = accel_stream.next() => {
                let msg = msg?;
                let (ts, x_mg, y_mg, z_mg): (u64, f64, f64, f64) = msg.body().deserialize()?;

                let (x, y, z) = (x_mg * MG_TO_MS2, y_mg * MG_TO_MS2, z_mg * MG_TO_MS2);

                let g = match &mut gravity {
                    None => {
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

                let (lx, ly, lz) = (x - g.0, y - g.1, z - g.2);
                let linear_ms2 = (lx * lx + ly * ly + lz * lz).sqrt();

                sender.send_blocking(ImuData { timestamp_us: ts, linear_ms2, gyro_rads }).ok();
            }

            else => break,
        }
    }

    Ok(())
}
