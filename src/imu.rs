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
use zbus::Connection;

/// Combined IMU update forwarded to the UI thread on every accelerometer sample.
pub struct ImuData {
    /// Magnitude of linear acceleration in m/s² after gravity removal (≥ 0).
    #[allow(dead_code)]
    pub linear_ms2: f64,
    /// Raw X component of linear acceleration in m/s² (gravity removed).
    /// Phone mounted upright: lateral (right = positive).
    pub accel_x_ms2: f64,
    /// Raw Y component of linear acceleration in m/s² (gravity removed).
    /// Phone mounted upright: vertical axis (gravity residual — not used for fusion).
    #[allow(dead_code)]
    pub accel_y_ms2: f64,
    /// Raw Z component of linear acceleration in m/s² (gravity removed).
    /// Phone mounted upright: longitudinal (forward = positive). Used for fusion and G-meter.
    pub accel_z_ms2: f64,
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
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
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
    let pid = std::process::id() as i64;

    // --- Accelerometer ---
    let accel_session = request_sensor(&conn, "accelerometersensor").await?;
    conn.call_method(
        Some("com.nokia.SensorService"),
        "/SensorManager/accelerometersensor",
        Some("local.AccelerometerSensor"),
        "setInterval",
        &(accel_session, 80i32),
    ).await.ok();
    start_sensor(&conn, "/SensorManager/accelerometersensor", "local.AccelerometerSensor", accel_session).await?;

    // --- Gyroscope (optional) ---
    let gyro_session: Option<i32> = async {
        let sid = request_sensor(&conn, "gyroscopesensor").await?;
        conn.call_method(
            Some("com.nokia.SensorService"),
            "/SensorManager/gyroscopesensor",
            Some("local.GyroscopeSensor"),
            "setInterval",
            &(sid, 80i32),
        ).await.ok();
        start_sensor(&conn, "/SensorManager/gyroscopesensor", "local.GyroscopeSensor", sid).await?;
        zbus::Result::Ok(sid)
    }.await.ok();

    if gyro_session.is_none() {
        eprintln!("IMU: gyroscopesensor unavailable, corner suppression disabled");
    }

    // sensorfw on this device (hybris adaptor) does not emit streaming
    // dataAvailable D-Bus signals. Poll the xyz property directly instead.
    // 80 ms interval → ~12.5 Hz, matches the sensor's default data rate.
    const POLL_MS: u64 = 80;
    const ALPHA: f64 = 0.990;         // LP filter α for gravity estimation (~8s time constant at 80ms poll)
    const MG_TO_MS2: f64 = 9.81 / 1000.0;
    const MDPS_TO_RADS: f64 = std::f64::consts::PI / 180_000.0;

    let mut gravity: Option<(f64, f64, f64)> = None;
    let mut last_accel_ts: u64 = 0;
    let _ = pid; // suppress unused warning if pid isn't used elsewhere

    loop {
        tokio::time::sleep(std::time::Duration::from_millis(POLL_MS)).await;

        // Poll gyroscope first so we have up-to-date angular velocity.
        let gyro_rads = if gyro_session.is_some() {
            match conn.call_method(
                Some("com.nokia.SensorService"),
                "/SensorManager/gyroscopesensor",
                Some("local.GyroscopeSensor"),
                "xyz",
                &(),
            ).await {
                Ok(reply) => {
                    if let Ok((_, gx, gy, gz)) = reply.body().deserialize::<(u64, f64, f64, f64)>() {
                        let rx = gx * MDPS_TO_RADS;
                        let ry = gy * MDPS_TO_RADS;
                        let rz = gz * MDPS_TO_RADS;
                        (rx * rx + ry * ry + rz * rz).sqrt()
                    } else { 0.0 }
                }
                Err(_) => 0.0,
            }
        } else {
            0.0
        };

        // Poll accelerometer.
        let reply = match conn.call_method(
            Some("com.nokia.SensorService"),
            "/SensorManager/accelerometersensor",
            Some("local.AccelerometerSensor"),
            "xyz",
            &(),
        ).await {
            Ok(r) => r,
            Err(_) => continue,
        };

        let (ts, x_mg, y_mg, z_mg): (u64, f64, f64, f64) = match reply.body().deserialize() {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Skip duplicate timestamps — sensor hasn't updated yet, hold last value.
        if ts == last_accel_ts {
            continue;
        }
        last_accel_ts = ts;

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

        let data = ImuData {
            linear_ms2,
            accel_x_ms2: lx,
            accel_y_ms2: ly,
            accel_z_ms2: lz,
            gyro_rads,
        };
        match sender.try_send(data) {
            Ok(_) => {}
            Err(async_channel::TrySendError::Full(_)) => {} // UI busy — drop this sample
            Err(async_channel::TrySendError::Closed(_)) => break, // receiver dropped — UI is gone
        }
    }

    Ok(())
}
