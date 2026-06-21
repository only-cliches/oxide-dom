use serde_json::json;
use winit::event::ElementState;
use winit::keyboard::KeyCode;

#[derive(Debug, Clone, Copy, Default)]
pub struct InputState {
    pub throttle: bool,
    pub brake: bool,
    pub steer_left: bool,
    pub steer_right: bool,
}

impl InputState {
    pub fn set_key(&mut self, code: KeyCode, state: ElementState) -> bool {
        let pressed = state == ElementState::Pressed;
        match code {
            KeyCode::ArrowUp => self.throttle = pressed,
            KeyCode::ArrowDown => self.brake = pressed,
            KeyCode::ArrowLeft => self.steer_left = pressed,
            KeyCode::ArrowRight => self.steer_right = pressed,
            _ => return false,
        }
        true
    }

    pub fn throttle_value(self) -> f32 {
        if self.throttle { 1.0 } else { 0.0 }
    }

    pub fn brake_value(self) -> f32 {
        if self.brake { 1.0 } else { 0.0 }
    }

    pub fn steering_value(self) -> f32 {
        match (self.steer_left, self.steer_right) {
            (true, false) => 1.0,
            (false, true) => -1.0,
            _ => 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CarState {
    pub x: f32,
    pub y: f32,
    pub heading: f32,
    pub speed: f32,
    pub steering: f32,
    pub throttle: f32,
    pub brake: f32,
}

impl Default for CarState {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            heading: 0.0,
            speed: 0.0,
            steering: 0.0,
            throttle: 0.0,
            brake: 0.0,
        }
    }
}

impl CarState {
    /// Advance the car by `dt` seconds. `max_speed` is the forward speed cap in
    /// m/s (driven by the HUD slider); reverse tops out at a fraction of it.
    pub fn update(&mut self, input: InputState, dt: f32, max_speed: f32) {
        let dt = dt.clamp(0.0, 0.05);
        self.throttle = input.throttle_value();
        self.brake = input.brake_value();
        self.steering = input.steering_value();

        // The down arrow brakes while the car is still rolling forward, then
        // smoothly transitions into a reverse throttle once it has stopped.
        let rolling_forward = self.speed > 0.5;
        let acceleration = 18.0 * self.throttle;
        let braking = if self.brake > 0.0 && rolling_forward {
            34.0
        } else {
            0.0
        };
        let reverse = if self.brake > 0.0 && !rolling_forward {
            -16.0
        } else {
            0.0
        };
        // Light drag so the slider-controlled cap (not aerodynamic terminal
        // velocity) is what limits top speed: accel 18 / drag 0.7 ≈ 25 m/s
        // terminal, comfortably above the 50 mph (~22 m/s) ceiling.
        let drag = self.speed * 0.7;

        self.speed += (acceleration - braking + reverse - drag) * dt;
        let reverse_limit = (max_speed * 0.45).min(13.0);
        self.speed = self.speed.clamp(-reverse_limit, max_speed);
        if self.speed.abs() < 0.03 && self.throttle == 0.0 && self.brake == 0.0 {
            self.speed = 0.0;
        }

        let turn_response = (self.speed.abs() / 14.0).clamp(0.15, 1.0);
        self.heading += self.steering * turn_response * self.speed.signum() * 1.65 * dt;

        let forward_x = self.heading.sin();
        let forward_y = self.heading.cos();
        self.x += forward_x * self.speed * dt;
        self.y += forward_y * self.speed * dt;
    }

    pub fn heading_degrees(self) -> f32 {
        self.heading.to_degrees().rem_euclid(360.0)
    }

    pub fn speed_mph(self) -> f32 {
        self.speed * 2.236_936
    }

    #[cfg(test)]
    fn step(&mut self, input: InputState, max_speed: f32, frames: u32) {
        for _ in 0..frames {
            self.update(input, 1.0 / 60.0, max_speed);
        }
    }

    pub fn telemetry_json(self, mode: &'static str) -> serde_json::Value {
        json!({
            "mode": mode,
            "speedMph": self.speed_mph(),
            "headingDeg": self.heading_degrees(),
            "steering": self.steering,
            "throttle": self.throttle,
            "brake": self.brake,
            "x": self.x,
            "y": self.y,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: f32 = 22.0; // m/s, ~50 mph

    fn down() -> InputState {
        InputState { brake: true, ..InputState::default() }
    }

    fn up() -> InputState {
        InputState { throttle: true, ..InputState::default() }
    }

    #[test]
    fn brake_from_forward_then_reverses() {
        let mut car = CarState { speed: 18.0, ..CarState::default() };
        // Holding down decelerates while rolling forward...
        car.step(down(), MAX, 1);
        assert!(car.speed < 18.0, "braking should slow the car, got {}", car.speed);
        // ...and eventually drives in reverse once stopped.
        car.step(down(), MAX, 240);
        assert!(car.speed < -2.0, "should reverse, got {}", car.speed);
    }

    #[test]
    fn reverse_is_capped_below_forward_limit() {
        let mut car = CarState::default();
        car.step(down(), MAX, 600);
        let reverse_limit = (MAX * 0.45).min(13.0);
        assert!(
            car.speed >= -reverse_limit - 0.01,
            "reverse exceeded cap: {} < {}",
            car.speed,
            -reverse_limit
        );
        assert!(car.speed < -4.0, "should be solidly reversing, got {}", car.speed);
    }

    #[test]
    fn throttle_respects_adjustable_max() {
        let mut car = CarState::default();
        car.step(up(), MAX, 600);
        assert!(car.speed <= MAX + 0.01, "exceeded max speed: {}", car.speed);
        assert!(car.speed > MAX - 2.0, "should approach max speed, got {}", car.speed);

        // A lower cap holds the car slower.
        let mut slow = CarState::default();
        slow.step(up(), 10.0, 600);
        assert!(slow.speed <= 10.01, "low cap not respected: {}", slow.speed);
    }
}
