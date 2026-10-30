//! `ROSES_LAUNCHER_RECORD=<out.mp4>` (a tool for videos, not a launcher
//! feature): the launcher draws itself off-screen, as for
//! `ROSES_LAUNCHER_CAPTURE`, frame by frame into an MP4 (the games' segment
//! format: 1920x1080, 60 fps, AAC 48 kHz stereo, here silent), with a drawn
//! mouse cursor following a script (`ROSES_LAUNCHER_RECORD_SCRIPT`).
//!
//! Script steps, comma-separated:
//! - `wait:S` (seconds),
//! - `move:TARGET:S` (the cursor glides there in S seconds), TARGET is
//!   `game:<id>` (a sidebar entry), `play`, `import`, `toggle` or `X;Y`
//!   (logical window pixels, 1000x600),
//! - `click` (pressed for 0.15 s, then released: a sidebar entry shows its
//!   game; Play is drawn pressed but nothing is started),
//! - `pad:up|down|left|right` (the pad's focus moves, as
//!   `ROSES_LAUNCHER_PAD_MOVES`).

/// The clip Jim's video starts with: idle, the cursor to DotR, Play.
pub const DEFAULT_SCRIPT: &str =
    "wait:1.2,move:game:roses:1.1,click,wait:0.5,move:play:0.9,wait:0.2,click,wait:1.3";

/// A click: the button stays down this long.
pub const PRESS_S: f64 = 0.15;
/// After the release before the next step.
pub const CLICK_S: f64 = 0.25;

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Game(String),
    Play,
    Import,
    Toggle,
    Point(f32, f32),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Wait(f64),
    Move(Target, f64),
    Click,
    Pad(String),
}

pub fn parse(script: &str) -> Result<Vec<Step>, String> {
    let seconds = |v: &str| -> Result<f64, String> {
        let s: f64 = v
            .trim()
            .parse()
            .map_err(|_| format!("record script: bad seconds {v:?}"))?;
        if (0.0..=600.0).contains(&s) {
            Ok(s)
        } else {
            Err(format!("record script: {s} s is out of range"))
        }
    };
    let mut steps = Vec::new();
    for word in script.split(',').map(str::trim).filter(|w| !w.is_empty()) {
        let parts: Vec<&str> = word.split(':').collect();
        let step = match parts.as_slice() {
            ["wait", s] => Step::Wait(seconds(s)?),
            ["click"] => Step::Click,
            ["pad", d]
                if ["up", "down", "left", "right"].contains(&d.to_ascii_lowercase().as_str()) =>
            {
                Step::Pad(d.to_ascii_lowercase())
            }
            ["move", "game", id, s] => Step::Move(Target::Game((*id).to_owned()), seconds(s)?),
            ["move", t, s] => {
                let target = match *t {
                    "play" => Target::Play,
                    "import" => Target::Import,
                    "toggle" => Target::Toggle,
                    xy => {
                        let (x, y) = xy
                            .split_once(';')
                            .ok_or_else(|| format!("record script: unknown target {xy:?}"))?;
                        let n = |v: &str| {
                            v.trim()
                                .parse::<f32>()
                                .map_err(|_| format!("record script: bad point {xy:?}"))
                        };
                        Target::Point(n(x)?, n(y)?)
                    }
                };
                Step::Move(target, seconds(s)?)
            }
            _ => return Err(format!("record script: cannot read {word:?}")),
        };
        steps.push(step);
    }
    Ok(steps)
}

/// The script's length in seconds.
pub fn duration(steps: &[Step]) -> f64 {
    steps
        .iter()
        .map(|s| match s {
            Step::Wait(t) | Step::Move(_, t) => *t,
            Step::Click => PRESS_S + CLICK_S,
            Step::Pad(_) => 0.0,
        })
        .sum()
}

/// A hand's glide: slow start and end (smoothstep).
pub fn ease(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_default_and_rejects_nonsense() {
        let steps = parse(DEFAULT_SCRIPT).unwrap();
        assert_eq!(steps[1], Step::Move(Target::Game("roses".into()), 1.1));
        assert_eq!(steps[2], Step::Click);
        assert!(
            (duration(&steps) - 6.0).abs() < 1e-9,
            "{}",
            duration(&steps)
        );
        assert_eq!(
            parse("move:500;300:1,pad:left").unwrap(),
            [
                Step::Move(Target::Point(500.0, 300.0), 1.0),
                Step::Pad("left".into())
            ]
        );
        assert!(parse("jump:1").is_err());
        assert!(parse("wait:-1").is_err());
        assert!(parse("move:nowhere:1").is_err());
        assert_eq!((ease(0.0), ease(0.5), ease(1.0)), (0.0, 0.5, 1.0));
    }
}
