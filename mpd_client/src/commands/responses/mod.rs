//! Typed responses to individual commands.

mod list;
mod playlist;
mod song;
mod sticker;

use std::{num::ParseIntError, str::FromStr, sync::Arc, time::Duration};

use bytes::Bytes;
use mpd_protocol::response::Frame;

pub use self::{
    list::List,
    playlist::Playlist,
    song::{Song, SongInQueue, SongRange},
    sticker::{StickerFind, StickerGet, StickerList},
};
use crate::commands::{ErrorKind, SingleMode, SongId, SongPosition, TypedResponseError};

type KeyValuePair = (Arc<str>, String);

/// Types which can be converted from a field value.
pub(crate) trait FromFieldValue: Sized {
    /// Convert the value.
    fn from_value(v: String, field: &'static str) -> Result<Self, TypedResponseError>;
}

impl FromFieldValue for bool {
    fn from_value(v: String, field: &'static str) -> Result<Self, TypedResponseError> {
        match &*v {
            "0" => Ok(false),
            "1" => Ok(true),
            _ => Err(TypedResponseError {
                field,
                kind: ErrorKind::InvalidValue(v),
            }),
        }
    }
}

impl FromFieldValue for Duration {
    fn from_value(v: String, field: &'static str) -> Result<Self, TypedResponseError> {
        parse_duration(field, v)
    }
}

impl FromFieldValue for PlayState {
    fn from_value(v: String, field: &'static str) -> Result<Self, TypedResponseError> {
        match &*v {
            "play" => Ok(PlayState::Playing),
            "pause" => Ok(PlayState::Paused),
            "stop" => Ok(PlayState::Stopped),
            _ => Err(TypedResponseError {
                field,
                kind: ErrorKind::InvalidValue(v),
            }),
        }
    }
}

fn parse_integer<I: FromStr<Err = ParseIntError>>(
    v: String,
    field: &'static str,
) -> Result<I, TypedResponseError> {
    v.parse::<I>().map_err(|e| TypedResponseError {
        field,
        kind: ErrorKind::MalformedInteger(e),
    })
}

impl FromFieldValue for u8 {
    fn from_value(v: String, field: &'static str) -> Result<Self, TypedResponseError> {
        parse_integer(v, field)
    }
}

impl FromFieldValue for u32 {
    fn from_value(v: String, field: &'static str) -> Result<Self, TypedResponseError> {
        parse_integer(v, field)
    }
}

impl FromFieldValue for u64 {
    fn from_value(v: String, field: &'static str) -> Result<Self, TypedResponseError> {
        parse_integer(v, field)
    }
}

impl FromFieldValue for usize {
    fn from_value(v: String, field: &'static str) -> Result<Self, TypedResponseError> {
        parse_integer(v, field)
    }
}

/// Get a *required* value for the given field, as the given type.
pub(crate) fn value<V: FromFieldValue>(
    frame: &mut Frame,
    field: &'static str,
) -> Result<V, TypedResponseError> {
    let value = frame.get(field).ok_or(TypedResponseError {
        field,
        kind: ErrorKind::Missing,
    })?;
    V::from_value(value, field)
}

/// Get an *optional* value for the given field, as the given type.
fn optional_value<V: FromFieldValue>(
    frame: &mut Frame,
    field: &'static str,
) -> Result<Option<V>, TypedResponseError> {
    match frame.get(field) {
        None => Ok(None),
        Some(v) => {
            let v = V::from_value(v, field)?;
            Ok(Some(v))
        }
    }
}

fn song_identifier(
    frame: &mut Frame,
    position_field: &'static str,
    id_field: &'static str,
) -> Result<Option<(SongPosition, SongId)>, TypedResponseError> {
    // The position field may or may not exist
    let position = match optional_value(frame, position_field)? {
        Some(p) => SongPosition(p),
        None => return Ok(None),
    };

    // ... but if the position field existed, the ID field must exist too
    let id = value(frame, id_field).map(SongId)?;

    Ok(Some((position, id)))
}

fn parse_duration<V: AsRef<str> + Into<String>>(
    field: &'static str,
    value: V,
) -> Result<Duration, TypedResponseError> {
    let v = value
        .as_ref()
        .parse::<f64>()
        .map_err(|e| TypedResponseError {
            field,
            kind: ErrorKind::MalformedFloat(e),
        })?;

    // Check if the parsed value is a reasonable duration, to avoid a panic from `from_secs_f64`
    if v >= 0.0 && v <= Duration::MAX.as_secs_f64() && v.is_finite() {
        Ok(Duration::from_secs_f64(v))
    } else {
        Err(TypedResponseError {
            field,
            kind: ErrorKind::InvalidValue(value.into()),
        })
    }
}

/// Possible playback states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum PlayState {
    Stopped,
    Playing,
    Paused,
}

/// Response to the [`status`] command.
///
/// See the [MPD documentation][status-command] for the specific meanings of the fields.
///
/// [`status`]: crate::commands::definitions::Status
/// [status-command]: https://www.musicpd.org/doc/html/protocol.html#command-status
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(missing_docs)]
#[non_exhaustive]
pub struct Status {
    pub volume: u8,
    pub state: PlayState,
    pub repeat: bool,
    pub random: bool,
    pub consume: bool,
    pub single: SingleMode,
    pub playlist_version: u32,
    pub playlist_length: usize,
    pub current_song: Option<(SongPosition, SongId)>,
    pub next_song: Option<(SongPosition, SongId)>,
    pub elapsed: Option<Duration>,
    pub duration: Option<Duration>,
    pub bitrate: Option<u64>,
    pub crossfade: Duration,
    pub update_job: Option<u64>,
    pub error: Option<String>,
    pub partition: Option<String>,
}

impl Status {
    pub(crate) fn from_frame(mut raw: Frame) -> Result<Self, TypedResponseError> {
        let single = match raw.get("single") {
            None => SingleMode::Disabled,
            Some(val) => match val.as_str() {
                "0" => SingleMode::Disabled,
                "1" => SingleMode::Enabled,
                "oneshot" => SingleMode::Oneshot,
                _ => {
                    return Err(TypedResponseError {
                        field: "single",
                        kind: ErrorKind::InvalidValue(val),
                    })
                }
            },
        };

        let duration = if let Some(val) = raw.get("duration") {
            Some(Duration::from_value(val, "duration")?)
        } else if let Some(time) = raw.get("time") {
            // Backwards compatibility with protocol versions <0.20
            if let Some((_, duration)) = time.split_once(':') {
                Some(Duration::from_value(duration.to_owned(), "time")?)
            } else {
                // No separator
                return Err(TypedResponseError {
                    field: "time",
                    kind: ErrorKind::InvalidValue(time),
                });
            }
        } else {
            None
        };

        let f = &mut raw;

        Ok(Self {
            volume: optional_value(f, "volume")?.unwrap_or(0),
            state: value(f, "state")?,
            repeat: value(f, "repeat")?,
            random: value(f, "random")?,
            consume: value(f, "consume")?,
            single,
            playlist_length: optional_value(f, "playlistlength")?.unwrap_or(0),
            playlist_version: optional_value(f, "playlist")?.unwrap_or(0),
            current_song: song_identifier(f, "song", "songid")?,
            next_song: song_identifier(f, "nextsong", "nextsongid")?,
            elapsed: optional_value(f, "elapsed")?,
            duration,
            bitrate: optional_value(f, "bitrate")?,
            crossfade: optional_value(f, "xfade")?.unwrap_or(Duration::ZERO),
            update_job: optional_value(f, "update_job")?,
            error: f.get("error"),
            partition: f.get("partition"),
        })
    }
}

/// Response to the [`stats`] command, containing general server statistics.
///
/// [`stats`]: crate::commands::definitions::Stats
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(missing_docs)]
#[non_exhaustive]
pub struct Stats {
    pub artists: u64,
    pub albums: u64,
    pub songs: u64,
    pub uptime: Duration,
    pub playtime: Duration,
    pub db_playtime: Duration,
    /// Raw server UNIX timestamp of last database update.
    pub db_last_update: u64,
}

impl Stats {
    pub(crate) fn from_frame(mut f: Frame) -> Result<Self, TypedResponseError> {
        let f = &mut f;
        Ok(Self {
            artists: value(f, "artists")?,
            albums: value(f, "albums")?,
            songs: value(f, "songs")?,
            uptime: value(f, "uptime")?,
            playtime: value(f, "playtime")?,
            db_playtime: value(f, "db_playtime")?,
            db_last_update: value(f, "db_update")?,
        })
    }
}

/// Response to the [`albumart`][crate::commands::AlbumArt] and
/// [`readpicture`][crate::commands::AlbumArtEmbedded] commands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlbumArt {
    /// The total size in bytes of the file.
    pub size: usize,
    /// The mime type, if known.
    pub mime: Option<String>,
    /// The raw data.
    data: Bytes,
}

impl AlbumArt {
    /// Get the data in the response.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub(crate) fn from_frame(mut frame: Frame) -> Result<Option<Self>, TypedResponseError> {
        let data = match frame.take_binary() {
            Some(d) => d.freeze(),
            None => return Ok(None),
        };

        Ok(Some(AlbumArt {
            size: value(&mut frame, "size")?,
            mime: frame.get("type"),
            data,
        }))
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;

    use super::*;

    #[test]
    fn duration_parsing() {
        assert_eq!(
            parse_duration("duration", "1.500").unwrap(),
            Duration::from_secs_f64(1.5)
        );
        assert_eq!(parse_duration("Time", "3").unwrap(), Duration::from_secs(3));

        // Error cases
        assert_matches!(parse_duration("duration", "-1"), Err(_));
        assert_matches!(parse_duration("duration", "NaN"), Err(_));
        assert_matches!(parse_duration("duration", "-1"), Err(_));
    }
}
