use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};

#[derive(Debug, Default)]
struct State {
    available: u64,
    complete: bool,
}

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Debug)]
pub struct GrowingFileReader {
    file: File,
    pos: u64,
    shared: Arc<Shared>,
}

#[derive(Debug)]
pub struct GrowingFileWriter {
    shared: Arc<Shared>,
}

pub fn growing_file(path: &Path) -> io::Result<(GrowingFileReader, GrowingFileWriter)> {
    let file = File::open(path)?;
    let shared = Arc::new(Shared::default());
    Ok((
        GrowingFileReader {
            file,
            pos: 0,
            shared: Arc::clone(&shared),
        },
        GrowingFileWriter { shared },
    ))
}

impl GrowingFileWriter {
    pub fn add_available(&self, bytes: u64) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.available = state.available.saturating_add(bytes);
        self.shared.changed.notify_all();
    }

    pub fn finish(&self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.complete = true;
        self.shared.changed.notify_all();
    }
}

impl Drop for GrowingFileWriter {
    fn drop(&mut self) {
        self.finish();
    }
}

impl Read for GrowingFileReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while self.pos >= state.available && !state.complete {
                state = self
                    .shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if self.pos >= state.available && state.complete {
                return Ok(0);
            }
            let readable = (state.available - self.pos).min(buf.len() as u64) as usize;
            drop(state);
            let read = self.file.read(&mut buf[..readable])?;
            self.pos = self.pos.saturating_add(read as u64);
            if read > 0 {
                return Ok(read);
            }
        }
    }
}

impl Seek for GrowingFileReader {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "streaming playback is not seekable yet",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn growing_reader_reads_available_bytes_then_eof() {
        let path =
            std::env::temp_dir().join(format!("furumi-growing-reader-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        std::fs::File::create(&path).unwrap();
        let (mut reader, writer) = growing_file(&path).unwrap();
        let mut output = std::fs::OpenOptions::new().write(true).open(&path).unwrap();

        output.write_all(b"fur").unwrap();
        writer.add_available(3);
        let mut first = [0u8; 3];
        reader.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"fur");

        output.write_all(b"umi").unwrap();
        writer.add_available(3);
        writer.finish();
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(&rest, b"umi");
        drop(output);
        let _ = std::fs::remove_file(&path);
    }
}
