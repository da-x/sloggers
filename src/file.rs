//! File logger.
use slog::{Drain, FnValue, Logger};
use slog_async::Async;
use slog_kvfilter::{KVFilter, KVFilterList};
use slog_term::{CompactFormat, FullFormat, PlainDecorator};
use std::fmt::Debug;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use misc::KVFilterParameters;
use misc::{module_and_line, timezone_to_timestamp_fn};
use types::{Format, Severity, SourceLocation, TimeZone};
use {Build, Config, Result};

/// A logger builder which build loggers that write log records to the specified file.
///
/// The resulting logger will work asynchronously (the default channel size is 1024).
#[derive(Debug)]
pub struct FileLoggerBuilder {
    format: Format,
    source_location: SourceLocation,
    timezone: TimeZone,
    level: Severity,
    appender: FileAppender,
    channel_size: usize,
    kvfilterparameters: Option<KVFilterParameters>,
}
impl FileLoggerBuilder {
    /// Makes a new `FileLoggerBuilder` instance.
    ///
    /// This builder will create a logger which uses `path` as
    /// the output destination of the log records.
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        FileLoggerBuilder {
            format: Format::default(),
            source_location: SourceLocation::default(),
            timezone: TimeZone::default(),
            level: Severity::default(),
            appender: FileAppender::new(path),
            channel_size: 1024,
            kvfilterparameters: None,
        }
    }

    /// Sets the format of log records.
    pub fn format(&mut self, format: Format) -> &mut Self {
        self.format = format;
        self
    }

    /// Sets the source code location type this logger will use.
    pub fn source_location(&mut self, source_location: SourceLocation) -> &mut Self {
        self.source_location = source_location;
        self
    }

    /// Sets the time zone which this logger will use.
    pub fn timezone(&mut self, timezone: TimeZone) -> &mut Self {
        self.timezone = timezone;
        self
    }

    /// Sets the log level of this logger.
    pub fn level(&mut self, severity: Severity) -> &mut Self {
        self.level = severity;
        self
    }

    /// Sets the size of the asynchronous channel of this logger.
    pub fn channel_size(&mut self, channel_size: usize) -> &mut Self {
        self.channel_size = channel_size;
        self
    }

    /// Sets [`KVFilter`].
    ///
    /// [`KVFilter`]: https://docs.rs/slog-kvfilter/0.6/slog_kvfilter/struct.KVFilter.html
    pub fn kvfilter(
        &mut self,
        level: Severity,
        only_pass_any_on_all_keys: Option<KVFilterList>,
        always_suppress_any: Option<KVFilterList>,
    ) -> &mut Self {
        self.kvfilterparameters = Some(KVFilterParameters {
            severity: level,
            only_pass_any_on_all_keys,
            always_suppress_any,
        });
        self
    }

    /// By default, logger just appends log messages to file.
    /// If this method called, logger truncates the file to 0 length when opening.
    pub fn truncate(&mut self) -> &mut Self {
        self.appender.truncate = true;
        self
    }

    /// TODO: doc
    ///
    /// The default value is `std::u64::MAX`.
    pub fn rotate_size(&mut self, size: u64) -> &mut Self {
        self.appender.rotate_size = size;
        self
    }

    /// Sets the maximum number of rotated log files to keep.
    ///
    /// Older rotated log files get pruned.
    ///
    /// The default value is `8`.
    pub fn rotate_keep(&mut self, count: usize) -> &mut Self {
        self.appender.rotate_keep = count;
        self
    }

    fn build_with_drain<D>(&self, drain: D) -> Logger
    where
        D: Drain + Send + 'static,
        D::Err: Debug,
    {
        // async inside, level and key value filters outside for speed
        let drain = Async::new(drain.fuse())
            .chan_size(self.channel_size)
            .build()
            .fuse();

        if let Some(ref p) = self.kvfilterparameters {
            let kvdrain = KVFilter::new(drain, p.severity.as_level())
                .always_suppress_any(p.always_suppress_any.clone())
                .only_pass_any_on_all_keys(p.only_pass_any_on_all_keys.clone());

            let drain = self.level.set_level_filter(kvdrain.fuse());

            match self.source_location {
                SourceLocation::None => Logger::root(drain.fuse(), o!()),
                SourceLocation::ModuleAndLine => {
                    Logger::root(drain.fuse(), o!("module" => FnValue(module_and_line)))
                }
            }
        } else {
            let drain = self.level.set_level_filter(drain.fuse());

            match self.source_location {
                SourceLocation::None => Logger::root(drain.fuse(), o!()),
                SourceLocation::ModuleAndLine => {
                    Logger::root(drain.fuse(), o!("module" => FnValue(module_and_line)))
                }
            }
        }
    }
}

impl Build for FileLoggerBuilder {
    fn build(&self) -> Result<Logger> {
        let decorator = PlainDecorator::new(self.appender.clone());
        let timestamp = timezone_to_timestamp_fn(self.timezone);
        let logger = match self.format {
            Format::Full => {
                let format = FullFormat::new(decorator).use_custom_timestamp(timestamp);
                self.build_with_drain(format.build())
            }
            Format::Compact => {
                let format = CompactFormat::new(decorator).use_custom_timestamp(timestamp);
                self.build_with_drain(format.build())
            }
        };
        Ok(logger)
    }
}

#[derive(Debug)]
struct FileAppender {
    path: PathBuf,
    file: Option<File>,
    truncate: bool,
    written_size: u64,
    rotate_size: u64,
    rotate_keep: usize,
}
impl Clone for FileAppender {
    fn clone(&self) -> Self {
        FileAppender {
            path: self.path.clone(),
            file: None,
            truncate: self.truncate,
            written_size: 0,
            rotate_size: self.rotate_size,
            rotate_keep: self.rotate_keep,
        }
    }
}
impl FileAppender {
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        use std::u64;

        FileAppender {
            path: path.as_ref().to_path_buf(),
            file: None,
            truncate: false,
            written_size: 0,
            rotate_size: u64::MAX,
            rotate_keep: 8,
        }
    }
    fn reopen_if_needed(&mut self) -> io::Result<()> {
        if !self.path.exists() {
            let mut file_builder = OpenOptions::new();
            file_builder.create(true);
            if self.truncate {
                file_builder.truncate(true);
            }
            let file = file_builder
                .append(!self.truncate)
                .write(true)
                .open(&self.path)?;
            self.written_size = file.metadata()?.len();
            self.file = Some(file);
        }
        Ok(())
    }
    fn rotate(&mut self) -> io::Result<()> {
        let _ = self.file.take();

        for i in (1..self.rotate_keep + 1).rev() {
            let from = self.rotated_path(i)?;
            let to = self.rotated_path(i + 1)?;
            if from.exists() {
                fs::rename(from, to)?;
            }
        }
        if self.path.exists() {
            fs::rename(&self.path, self.rotated_path(1)?)?;
        }

        let delete_path = self.rotated_path(self.rotate_keep + 1)?;
        if delete_path.exists() {
            fs::remove_file(delete_path)?;
        }

        self.written_size = 0;
        self.reopen_if_needed()?;

        Ok(())
    }
    fn rotated_path(&self, i: usize) -> io::Result<PathBuf> {
        let path = self.path.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Non UTF-8 log file path: {:?}", self.path),
            )
        })?;
        Ok(PathBuf::from(format!("{}.{}", path, i)))
    }
}
impl Write for FileAppender {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.reopen_if_needed()?;
        let size = if let Some(ref mut f) = self.file {
            f.write(buf)?
        } else {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("Cannot open file: {:?}", self.path),
            ));
        };

        self.written_size += size as u64;
        if self.written_size >= self.rotate_size {
            self.rotate()?;
        }
        Ok(size)
    }
    fn flush(&mut self) -> io::Result<()> {
        if let Some(ref mut f) = self.file {
            f.flush()?;
        }
        Ok(())
    }
}

/// The configuration of `FileLoggerBuilder`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct FileLoggerConfig {
    /// Log level.
    #[serde(default)]
    pub level: Severity,

    /// Log record format.
    #[serde(default)]
    pub format: Format,

    /// Source code location
    #[serde(default)]
    pub source_location: SourceLocation,

    /// Time Zone.
    #[serde(default)]
    pub timezone: TimeZone,

    /// Log file path.
    pub path: PathBuf,

    /// Asynchronous channel size
    #[serde(default = "default_channel_size")]
    pub channel_size: usize,

    /// Truncate the file or not
    #[serde(default)]
    pub truncate: bool,
}
impl Config for FileLoggerConfig {
    type Builder = FileLoggerBuilder;
    fn try_to_builder(&self) -> Result<Self::Builder> {
        let mut builder = FileLoggerBuilder::new(&self.path);
        builder.level(self.level);
        builder.format(self.format);
        builder.source_location(self.source_location);
        builder.timezone(self.timezone);
        builder.channel_size(self.channel_size);
        if self.truncate {
            builder.truncate();
        }
        Ok(builder)
    }
}

fn default_channel_size() -> usize {
    1024
}

#[cfg(test)]
mod tests {
    // TODO: log rotation test
}
