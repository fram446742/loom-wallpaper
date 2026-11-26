//! Time-based rules, playlists, random selection and cross-fade math.
//!
//! Everything here is pure: callers pass a wall clock minute, a Unix second and
//! a deterministic seed, so the behaviour is testable without a compositor or
//! a timer.

use serde::{Deserialize, Serialize};

/// Parses `HH:MM` into minutes since midnight (0..1440).
pub fn parse_hhmm(text: &str) -> Option<u32> {
    let (hour, minute) = text.trim().split_once(':')?;
    let hour: u32 = hour.trim().parse().ok()?;
    let minute: u32 = minute.trim().parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some(hour * 60 + minute)
}

/// A day/night image range. `start`/`end` are `HH:MM`; a range whose end is
/// before its start wraps around midnight (`22:00`–`07:00`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeRule {
    pub start: String,
    pub end: String,
    pub image: String,
}

impl TimeRule {
    pub fn matches(&self, minute_of_day: u32) -> bool {
        let (Some(start), Some(end)) = (parse_hhmm(&self.start), parse_hhmm(&self.end)) else {
            return false;
        };
        let minute = minute_of_day % 1440;
        if start == end {
            return true;
        }
        if start < end {
            minute >= start && minute < end
        } else {
            minute >= start || minute < end
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if parse_hhmm(&self.start).is_none() {
            return Err(format!("time rule start `{}` must be HH:MM", self.start));
        }
        if parse_hhmm(&self.end).is_none() {
            return Err(format!("time rule end `{}` must be HH:MM", self.end));
        }
        if self.image.trim().is_empty() {
            return Err("time rule needs an image".to_owned());
        }
        Ok(())
    }
}

/// An ordered or shuffled image rotation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Playlist {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    /// How long each image stays on screen.
    pub interval_secs: u64,
    /// Rotate in a deterministic shuffled order instead of list order.
    pub shuffle: bool,
}

impl Playlist {
    pub fn interval(&self) -> u64 {
        self.interval_secs.max(1)
    }

    /// Image shown at `now` (Unix seconds) with a deterministic shuffle seed.
    pub fn image_at(&self, now: u64, seed: u64) -> Option<&String> {
        if self.images.is_empty() {
            return None;
        }
        let slot = (now / self.interval()) as usize;
        if !self.shuffle {
            return self.images.get(slot % self.images.len());
        }
        let mut order: Vec<usize> = (0..self.images.len()).collect();
        shuffle(&mut order, seed);
        order
            .get(slot % order.len())
            .and_then(|index| self.images.get(*index))
    }
}

/// Deterministic Fisher-Yates shuffle.
pub fn shuffle<T>(items: &mut [T], seed: u64) {
    let mut state = seed | 1;
    for index in (1..items.len()).rev() {
        state = xorshift(state);
        let swap = (state as usize) % (index + 1);
        items.swap(index, swap);
    }
}

fn xorshift(mut state: u64) -> u64 {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
}

/// SplitMix64 finalizer: decorrelates sequential slots before taking a modulo.
fn splitmix64(mut state: u64) -> u64 {
    state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    state = (state ^ (state >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state = (state ^ (state >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

/// Picks one index deterministically for `seed`.
pub fn random_index(len: usize, seed: u64) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some((splitmix64(seed) as usize) % len)
}

/// Randomly picks one of `candidates`, changing only when the time slot
/// (`now / interval`) changes, so a scene does not flicker every redraw.
pub fn random_candidate(candidates: &[String], now: u64, interval_secs: u64) -> Option<&String> {
    if candidates.is_empty() {
        return None;
    }
    let slot = now / interval_secs.max(1);
    let index = random_index(candidates.len(), slot ^ 0x9e37_79b9_7f4a_7c15)?;
    candidates.get(index)
}

/// Transition easing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Easing {
    Linear,
    In,
    Out,
    #[default]
    InOut,
}

impl Easing {
    /// Maps linear progress 0..1 to eased progress 0..1.
    pub fn apply(self, progress: f64) -> f64 {
        let t = progress.clamp(0.0, 1.0);
        match self {
            Self::Linear => t,
            Self::In => t * t,
            Self::Out => 1.0 - (1.0 - t) * (1.0 - t),
            Self::InOut => {
                if t < 0.5 {
                    2.0 * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(2) / 2.0
                }
            }
        }
    }
}

/// Cross-fade configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TransitionSpec {
    pub duration_ms: u64,
    pub easing: Easing,
}

impl Default for TransitionSpec {
    fn default() -> Self {
        Self {
            duration_ms: 500,
            easing: Easing::InOut,
        }
    }
}

impl TransitionSpec {
    /// Linear progress at `elapsed_ms`.
    pub fn progress(&self, elapsed_ms: u64) -> f64 {
        if self.duration_ms == 0 {
            return 1.0;
        }
        (elapsed_ms as f64 / self.duration_ms as f64).clamp(0.0, 1.0)
    }

    /// Eased progress at `elapsed_ms`.
    pub fn eased(&self, elapsed_ms: u64) -> f64 {
        self.easing.apply(self.progress(elapsed_ms))
    }
}

/// Blends two XRGB8888 buffers, `alpha` 0 = `from`, 1 = `to`.
///
/// Buffers with different lengths are blended over the shorter length; callers
/// only cross-fade buffers of the same geometry.
pub fn cross_fade(from: &[u8], to: &[u8], alpha: f64) -> Vec<u8> {
    let alpha = alpha.clamp(0.0, 1.0);
    let len = from.len().min(to.len());
    let inverse = 1.0 - alpha;
    let mut out = Vec::with_capacity(len);
    for (from, to) in from.iter().zip(to.iter()).take(len) {
        let value = (*from as f64 * inverse + *to as f64 * alpha).round();
        out.push(value.clamp(0.0, 255.0) as u8);
    }
    out
}

/// A cross-fade in progress, independent of Wayland.
#[derive(Debug, Clone)]
pub struct TransitionState {
    pub spec: TransitionSpec,
    pub from: Vec<u8>,
    pub to: Vec<u8>,
    pub elapsed_ms: u64,
}

impl TransitionState {
    pub fn new(spec: TransitionSpec, from: Vec<u8>, to: Vec<u8>) -> Self {
        Self {
            spec,
            from,
            to,
            elapsed_ms: 0,
        }
    }

    /// Advances by `delta_ms` and returns the blended frame.
    pub fn advance(&mut self, delta_ms: u64) -> Vec<u8> {
        self.elapsed_ms = self.elapsed_ms.saturating_add(delta_ms);
        self.frame()
    }

    pub fn frame(&self) -> Vec<u8> {
        let alpha = self.spec.eased(self.elapsed_ms);
        cross_fade(&self.from, &self.to, alpha)
    }

    pub fn done(&self) -> bool {
        self.elapsed_ms >= self.spec.duration_ms
    }
}

/// Frame interval used while a native cross-fade is running.
pub const FRAME_MS: u64 = 16;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_matches_day_and_night_ranges() {
        assert_eq!(parse_hhmm("07:30"), Some(450));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("7"), None);

        let day = TimeRule {
            start: "07:00".to_owned(),
            end: "19:00".to_owned(),
            image: "/day.png".to_owned(),
        };
        assert!(day.matches(7 * 60));
        assert!(day.matches(18 * 60 + 59));
        assert!(!day.matches(19 * 60));
        assert!(!day.matches(6 * 60 + 59));

        let night = TimeRule {
            start: "22:00".to_owned(),
            end: "06:00".to_owned(),
            image: "/night.png".to_owned(),
        };
        assert!(night.matches(22 * 60));
        assert!(night.matches(0));
        assert!(night.matches(5 * 60 + 59));
        assert!(!night.matches(6 * 60));
        assert!(!night.matches(12 * 60));

        assert!(day.validate().is_ok());
        assert!(night.validate().is_ok());
        assert!(
            TimeRule {
                start: "25:00".to_owned(),
                ..day
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn ordered_playlist_advances_by_interval() {
        let playlist = Playlist {
            images: vec!["/a".to_owned(), "/b".to_owned(), "/c".to_owned()],
            interval_secs: 10,
            shuffle: false,
        };
        assert_eq!(playlist.image_at(0, 7).map(String::as_str), Some("/a"));
        assert_eq!(playlist.image_at(9, 7).map(String::as_str), Some("/a"));
        assert_eq!(playlist.image_at(10, 7).map(String::as_str), Some("/b"));
        assert_eq!(playlist.image_at(20, 7).map(String::as_str), Some("/c"));
        assert_eq!(playlist.image_at(30, 7).map(String::as_str), Some("/a"));

        let empty = Playlist::default();
        assert!(empty.image_at(0, 0).is_none());
    }

    #[test]
    fn shuffled_playlist_is_deterministic_and_covers_every_image() {
        let playlist = Playlist {
            images: vec!["/a".to_owned(), "/b".to_owned(), "/c".to_owned()],
            interval_secs: 1,
            shuffle: true,
        };
        let first: Vec<&str> = (0..3)
            .map(|slot| playlist.image_at(slot, 42).unwrap().as_str())
            .collect();
        let second: Vec<&str> = (0..3)
            .map(|slot| playlist.image_at(slot, 42).unwrap().as_str())
            .collect();
        assert_eq!(first, second);
        let mut sorted = first.clone();
        sorted.sort();
        assert_eq!(sorted, ["/a", "/b", "/c"]);
        // A different seed changes the order (with overwhelming probability).
        let other: Vec<&str> = (0..3)
            .map(|slot| playlist.image_at(slot, 7).unwrap().as_str())
            .collect();
        assert_ne!(first, other);
    }

    #[test]
    fn random_selection_is_stable_within_a_slot() {
        let images = vec![
            "/a".to_owned(),
            "/b".to_owned(),
            "/c".to_owned(),
            "/d".to_owned(),
        ];
        let first = random_candidate(&images, 100, 30).unwrap();
        let same = random_candidate(&images, 119, 30).unwrap();
        assert_eq!(first, same);
        assert!(images.iter().any(|_| true));
        assert!(random_candidate(&[], 0, 30).is_none());
        let mut indexes = std::collections::BTreeSet::new();
        for slot in 0..20 {
            indexes.insert(random_candidate(&images, slot * 30, 30).unwrap().clone());
        }
        assert!(indexes.len() > 1);
    }

    #[test]
    fn easing_is_monotonic_and_bounded() {
        for easing in [Easing::Linear, Easing::In, Easing::Out, Easing::InOut] {
            assert_eq!(easing.apply(0.0), 0.0);
            assert_eq!(easing.apply(1.0), 1.0);
            let mut previous = 0.0;
            for step in 0..=10 {
                let value = easing.apply(step as f64 / 10.0);
                assert!(value >= previous, "{easing:?} went backwards");
                assert!((0.0..=1.0).contains(&value));
                previous = value;
            }
        }
        assert_eq!(Easing::In.apply(0.5), 0.25);
        assert!((Easing::InOut.apply(0.25) - 0.125).abs() < 1e-9);
    }

    #[test]
    fn cross_fade_interpolates_and_transition_finishes() {
        let from = vec![0u8, 0, 0, 255];
        let to = vec![255u8, 100, 50, 255];
        assert_eq!(cross_fade(&from, &to, 0.0), from);
        assert_eq!(cross_fade(&from, &to, 1.0), to);
        let half = cross_fade(&from, &to, 0.5);
        assert_eq!(half[0], 128);
        assert_eq!(half[1], 50);

        let spec = TransitionSpec {
            duration_ms: 100,
            easing: Easing::Linear,
        };
        let mut transition = TransitionState::new(spec, from.clone(), to.clone());
        assert!(!transition.done());
        let frame = transition.advance(50);
        assert_eq!(frame, cross_fade(&from, &to, 0.5));
        transition.advance(60);
        assert!(transition.done());
        assert_eq!(transition.frame(), to);
        // A zero duration is immediately complete.
        assert!(
            TransitionSpec {
                duration_ms: 0,
                easing: Easing::Linear
            }
            .progress(0)
            .eq(&1.0)
        );
    }
}
