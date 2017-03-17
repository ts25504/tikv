// Copyright 2017 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::error;
use std::io::{self, Write, ErrorKind, Read};
use std::fmt::{self, Formatter, Display};
use std::fs::{self, Metadata};
use std::collections::hash_map::Entry;
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::path::Path;
use std::result;
use std::str;

use protobuf::Message;
use rocksdb::DB;
use kvproto::eraftpb::Snapshot as RaftSnapshot;
use kvproto::metapb::Region;
use kvproto::raft_serverpb::RaftSnapshotData;

use raftstore::Result as RaftStoreResult;
use raftstore::store::Msg;
use storage::CF_RAFT;
use util::transport::SendCh;
use util::{HandyRwLock, HashMap};

use super::engine::Snapshot as DbSnapshot;
use super::peer_storage::JOB_STATUS_CANCELLING;

/// Name prefix for the self-generated snapshot file.
const SNAP_GEN_PREFIX: &'static str = "gen";
/// Name prefix for the received snapshot file.
const SNAP_REV_PREFIX: &'static str = "rev";

const TMP_FILE_SUFFIX: &'static str = ".tmp";
const SNAP_FILE_SUFFIX: &'static str = ".snap";
const SST_FILE_SUFFIX: &'static str = ".sst";

quick_error! {
    #[derive(Debug)]
    pub enum Error {
        Abort {
            description("abort")
            display("abort")
        }
        Other(err: Box<error::Error + Sync + Send>) {
            from()
            cause(err.as_ref())
            description(err.description())
            display("snap failed {:?}", err)
        }
    }
}

pub type Result<T> = result::Result<T, Error>;

#[inline]
pub fn check_abort(status: &AtomicUsize) -> Result<()> {
    if status.load(Ordering::Relaxed) == JOB_STATUS_CANCELLING {
        return Err(Error::Abort);
    }
    Ok(())
}

#[derive(Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct SnapKey {
    pub region_id: u64,
    pub term: u64,
    pub idx: u64,
}

impl SnapKey {
    #[inline]
    pub fn new(region_id: u64, term: u64, idx: u64) -> SnapKey {
        SnapKey {
            region_id: region_id,
            term: term,
            idx: idx,
        }
    }

    pub fn from_region_snap(region_id: u64, snap: &RaftSnapshot) -> SnapKey {
        let index = snap.get_metadata().get_index();
        let term = snap.get_metadata().get_term();
        SnapKey::new(region_id, term, index)
    }

    pub fn from_snap(snap: &RaftSnapshot) -> io::Result<SnapKey> {
        let mut snap_data = RaftSnapshotData::new();
        if let Err(e) = snap_data.merge_from_bytes(snap.get_data()) {
            return Err(io::Error::new(ErrorKind::Other, e));
        }

        Ok(SnapKey::from_region_snap(snap_data.get_region().get_id(), snap))
    }
}

impl Display for SnapKey {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{}_{}_{}", self.region_id, self.term, self.idx)
    }
}

#[derive(Default)]
pub struct BuildContext {
    pub snapshot_size: u64,
    pub snapshot_kv_count: usize,
}

impl BuildContext {
    pub fn new() -> BuildContext {
        BuildContext { ..Default::default() }
    }
}

pub struct ApplyOptions {
    pub db: Arc<DB>,
    pub region: Region,
    pub abort: Arc<AtomicUsize>,
    pub write_batch_size: usize,
}

/// `Snapshot` is a trait for snapshot.
/// It's used in these scenarios:
///   1. build local snapshot
///   1. read local snapshot and then replicate it to remote raftstores
///   2. receive snapshot from remote raftstore and write it to local storage
///   3. snapshot gc
///   4. apply snapshot
pub trait Snapshot: Read + Write + Send {
    fn build(&mut self,
             snap: &DbSnapshot,
             region: &Region,
             snap_data: &mut RaftSnapshotData,
             context: &mut BuildContext)
             -> RaftStoreResult<()>;
    fn path(&self) -> &str;
    fn exists(&self) -> bool;
    fn delete(&self);
    fn meta(&self) -> io::Result<Metadata>;
    fn total_size(&self) -> io::Result<u64>;
    fn save(&mut self) -> io::Result<()>;
    fn apply(&mut self, options: ApplyOptions) -> Result<()>;
}

// A helper function to copy snapshot.
// Only used in tests.
pub fn copy_snapshot(mut from: Box<Snapshot>, mut to: Box<Snapshot>) -> io::Result<()> {
    if !to.exists() {
        try!(io::copy(&mut from, &mut to));
        try!(to.save());
    }
    Ok(())
}

fn need_to_pack(cf: &str) -> bool {
    // Data in CF_RAFT should be excluded for a snapshot.
    // Check CF_RAFT here and skip it, even though CF_RAFT should not occur
    // in the range [begin_key, end_key) of a RocksDB snapshot usually.
    cf != CF_RAFT
}

mod v1 {
    use std::cmp;
    use std::io::{self, Read, Write, ErrorKind};
    use std::fs::{self, File, OpenOptions, Metadata};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, RwLock};
    use std::str;
    use std::time::Instant;
    use crc::crc32::{self, Digest, Hasher32};
    use byteorder::{BigEndian, WriteBytesExt, ReadBytesExt};
    use rocksdb::{Writable, WriteBatch};
    use kvproto::metapb::Region;
    use kvproto::raft_serverpb::RaftSnapshotData;
    use util::{HandyRwLock, rocksdb, duration_to_sec};
    use util::codec::bytes::{BytesEncoder, CompactBytesDecoder};
    use raftstore::Result as RaftStoreResult;

    use super::super::engine::{Snapshot as DbSnapshot, Iterable};
    use super::super::keys::{self, enc_start_key, enc_end_key};
    use super::super::util;
    use super::super::metrics::{SNAPSHOT_CF_KV_COUNT, SNAPSHOT_BUILD_TIME_HISTOGRAM};
    use super::{SNAP_GEN_PREFIX, SNAP_REV_PREFIX, TMP_FILE_SUFFIX, SNAP_FILE_SUFFIX, Result,
                SnapKey, Snapshot, BuildContext, ApplyOptions, check_abort, need_to_pack};

    pub const SNAPSHOT_VERSION: u64 = 1;
    pub const CRC32_BYTES_COUNT: usize = 4;
    const DEFAULT_READ_BUFFER_SIZE: usize = 4096;

    /// A structure represents the snapshot.
    ///
    /// All changes to the file will be written to `tmp_file` first, and use
    /// `save` method to make them persistent. When saving a crc32 checksum
    /// will be appended to the file end automatically.
    pub struct Snap {
        file: PathBuf,
        digest: Option<Digest>,
        size_track: Arc<RwLock<u64>>,
        // File is the file obj represent the tmpfile, string is the actual path to
        // tmpfile.
        tmp_file: Option<(File, String)>,
        reader: Option<File>,
    }

    impl Snap {
        pub fn new_for_writing<T: Into<PathBuf>>(dir: T,
                                                 size_track: Arc<RwLock<u64>>,
                                                 is_sending: bool,
                                                 key: &SnapKey)
                                                 -> io::Result<Snap> {
            let mut path = dir.into();
            try!(Snap::prepare_path(&mut path, is_sending, key));

            let mut f = Snap {
                file: path,
                digest: Some(Digest::new(crc32::IEEE)),
                size_track: size_track,
                tmp_file: None,
                reader: None,
            };
            try!(f.init_for_writing());
            Ok(f)
        }

        pub fn new_for_reading<T: Into<PathBuf>>(dir: T,
                                                 size_track: Arc<RwLock<u64>>,
                                                 is_sending: bool,
                                                 key: &SnapKey)
                                                 -> io::Result<Snap> {
            let mut path = dir.into();
            try!(Snap::prepare_path(&mut path, is_sending, key));

            let f = Snap {
                file: path,
                digest: None,
                size_track: size_track,
                tmp_file: None,
                reader: None,
            };
            Ok(f)
        }

        fn prepare_path(path: &mut PathBuf, is_sending: bool, key: &SnapKey) -> io::Result<()> {
            if !path.exists() {
                try!(fs::create_dir_all(path.as_path()));
            }
            let prefix = if is_sending {
                SNAP_GEN_PREFIX
            } else {
                SNAP_REV_PREFIX
            };
            let file_name = format!("{}_{}{}", prefix, key, SNAP_FILE_SUFFIX);
            path.push(&file_name);
            Ok(())
        }

        fn init_for_writing(&mut self) -> io::Result<()> {
            if self.exists() || self.tmp_file.is_some() {
                return Ok(());
            }

            let tmp_path = format!("{}{}", self.path(), TMP_FILE_SUFFIX);
            let tmp_f = try!(OpenOptions::new().write(true).create_new(true).open(&tmp_path));
            self.tmp_file = Some((tmp_f, tmp_path));
            Ok(())
        }

        /// Get a validation reader.
        fn get_validation_reader(&self) -> io::Result<SnapValidationReader> {
            SnapValidationReader::open(self.path())
        }

        fn try_delete(&self) -> io::Result<()> {
            debug!("deleting {}", self.path());
            if !self.exists() {
                return Ok(());
            }
            let size = try!(self.meta()).len();
            try!(fs::remove_file(self.path()));
            let mut size_track = self.size_track.wl();
            *size_track = size_track.saturating_sub(size);
            Ok(())
        }

        /// Same as `save`, but will automatically append the checksum to
        /// the end of file.
        pub fn save_with_checksum(&mut self) -> io::Result<()> {
            self.save_impl(true)
        }

        fn save_impl(&mut self, append_checksum: bool) -> io::Result<()> {
            debug!("saving to {}", self.file.as_path().display());
            if let Some((mut f, path)) = self.tmp_file.take() {
                if append_checksum {
                    try!(f.write_u32::<BigEndian>(self.digest.as_ref().unwrap().sum32()));
                }
                try!(f.flush());
                let file_len = try!(fs::metadata(&path)).len();
                let mut size_track = self.size_track.wl();
                try!(fs::rename(path, self.file.as_path()));
                *size_track = size_track.saturating_add(file_len);
            }
            Ok(())
        }

        fn do_build(&mut self,
                    snap: &DbSnapshot,
                    region: &Region,
                    context: &mut BuildContext)
                    -> RaftStoreResult<()> {
            if self.exists() {
                match self.get_validation_reader().and_then(|r| r.validate()) {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        error!("[region {}] file {} is invalid, will rebuild: {:?}",
                               region.get_id(),
                               self.path(),
                               e);
                        try!(self.try_delete());
                        try!(self.init_for_writing());
                    }
                }
            }

            let mut snap_key_count = 0;
            let (begin_key, end_key) = (enc_start_key(region), enc_end_key(region));
            for cf in snap.cf_names() {
                if !need_to_pack(cf) {
                    continue;
                }
                box_try!(self.encode_compact_bytes(cf.as_bytes()));
                let mut cf_key_count = 0;
                try!(snap.scan_cf(cf,
                                  &begin_key,
                                  &end_key,
                                  false,
                                  &mut |key, value| {
                                      cf_key_count += 1;
                                      try!(self.encode_compact_bytes(key));
                                      try!(self.encode_compact_bytes(value));
                                      Ok(true)
                                  }));
                // use an empty byte array to indicate that cf reaches an end.
                box_try!(self.encode_compact_bytes(b""));
                snap_key_count += cf_key_count;
                SNAPSHOT_CF_KV_COUNT.with_label_values(&[cf]).observe(cf_key_count as f64);
                info!("[region {}] scan snapshot {}, cf {}, key count {}",
                      region.get_id(),
                      self.path(),
                      cf,
                      cf_key_count);
            }
            // use an empty byte array to indicate that kvpair reaches an end.
            box_try!(self.encode_compact_bytes(b""));
            try!(self.save_with_checksum());
            context.snapshot_kv_count = snap_key_count;
            Ok(())
        }
    }

    impl Snapshot for Snap {
        fn build(&mut self,
                 snap: &DbSnapshot,
                 region: &Region,
                 snap_data: &mut RaftSnapshotData,
                 context: &mut BuildContext)
                 -> RaftStoreResult<()> {
            let t = Instant::now();
            try!(self.do_build(snap, region, context));
            let size = try!(self.total_size());
            snap_data.set_file_size(size);
            snap_data.set_version(SNAPSHOT_VERSION);
            context.snapshot_size = size;
            SNAPSHOT_BUILD_TIME_HISTOGRAM.observe(duration_to_sec(t.elapsed()) as f64);
            info!("[region {}] scan snapshot {}, size {}, key count {}, takes {:?}",
                  region.get_id(),
                  self.path(),
                  size,
                  context.snapshot_kv_count,
                  t.elapsed());

            Ok(())
        }

        fn path(&self) -> &str {
            self.file.as_path().to_str().unwrap()
        }

        fn exists(&self) -> bool {
            self.file.exists() && self.file.is_file()
        }

        fn delete(&self) {
            if let Err(e) = self.try_delete() {
                error!("failed to delete {}: {:?}", self.path(), e);
            }
        }

        fn meta(&self) -> io::Result<Metadata> {
            self.file.metadata()
        }

        fn total_size(&self) -> io::Result<u64> {
            let meta = try!(self.meta());
            Ok(meta.len())
        }

        /// Use the content in temporary files replace the target file.
        ///
        /// Please note that this method can only be called once.
        fn save(&mut self) -> io::Result<()> {
            self.save_impl(false)
        }

        fn apply(&mut self, options: ApplyOptions) -> Result<()> {
            let mut reader = box_try!(self.get_validation_reader());
            loop {
                try!(check_abort(&options.abort));
                let cf = box_try!(reader.decode_compact_bytes());
                if cf.is_empty() {
                    break;
                }
                let handle = box_try!(rocksdb::get_cf_handle(&options.db, unsafe {
                    str::from_utf8_unchecked(&cf)
                }));
                let mut wb = WriteBatch::new();
                let mut batch_size = 0;
                loop {
                    try!(check_abort(&options.abort));
                    let key = box_try!(reader.decode_compact_bytes());
                    if key.is_empty() {
                        box_try!(options.db.write(wb));
                        break;
                    }
                    box_try!(util::check_key_in_region(keys::origin_key(&key), &options.region));
                    batch_size += key.len();
                    let value = box_try!(reader.decode_compact_bytes());
                    batch_size += value.len();
                    box_try!(wb.put_cf(handle, &key, &value));
                    if batch_size >= options.write_batch_size {
                        box_try!(options.db.write(wb));
                        wb = WriteBatch::new();
                        batch_size = 0;
                    }
                }
            }

            box_try!(reader.validate());
            Ok(())
        }
    }

    impl Read for Snap {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.reader.is_none() {
                let reader = try!(File::open(&self.file));
                self.reader = Some(reader);
            }
            self.reader.as_mut().unwrap().read(buf)
        }
    }

    impl Write for Snap {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.tmp_file.is_none() {
                return Ok(0);
            }
            let written = try!(self.tmp_file.as_mut().unwrap().0.write(buf));
            self.digest.as_mut().unwrap().write(&buf[..written]);
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.tmp_file.is_none() {
                return Ok(());
            }
            self.tmp_file.as_mut().unwrap().0.flush()
        }
    }

    impl Drop for Snap {
        fn drop(&mut self) {
            if let Some((_, path)) = self.tmp_file.take() {
                debug!("deleting {}", path);
                if let Err(e) = fs::remove_file(&path) {
                    warn!("failed to delete temporary file {}: {:?}", path, e);
                }
            }
        }
    }

    /// A reader that calculate checksum and verify it without read
    /// it from the beginning again.
    pub struct SnapValidationReader {
        reader: File,
        digest: Digest,
        left: usize,
        res: Option<u32>,
    }

    impl SnapValidationReader {
        /// Open the snap file located at specified path.
        fn open<P: AsRef<Path>>(path: P) -> io::Result<SnapValidationReader> {
            let reader = try!(File::open(path.as_ref()));
            let digest = Digest::new(crc32::IEEE);
            let len = try!(reader.metadata()).len();
            if len < CRC32_BYTES_COUNT as u64 {
                return Err(io::Error::new(ErrorKind::InvalidInput,
                                          format!("file length {} < {}", len, CRC32_BYTES_COUNT)));
            }
            let left = len as usize - CRC32_BYTES_COUNT;
            Ok(SnapValidationReader {
                reader: reader,
                digest: digest,
                left: left,
                res: None,
            })
        }

        /// Validate the file
        ///
        /// If the reader is consumed after calling this method, no further data can be
        /// read from this reader again.
        pub fn validate(mut self) -> io::Result<()> {
            if self.res.is_none() {
                if self.left > 0 {
                    let cap = cmp::min(self.left, DEFAULT_READ_BUFFER_SIZE);
                    let mut buf = vec![0; cap];
                    while self.left > 0 {
                        let _ = try!(self.read(&mut buf));
                    }
                }
                self.res = Some(try!(self.reader.read_u32::<BigEndian>()));
            }
            if self.res.unwrap() != self.digest.sum32() {
                let msg = format!("crc not correct: {} != {}",
                                  self.res.unwrap(),
                                  self.digest.sum32());
                return Err(io::Error::new(ErrorKind::InvalidData, msg));
            }
            Ok(())
        }
    }

    impl Read for SnapValidationReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.left == 0 {
                return Ok(0);
            }
            let read = if buf.len() < self.left {
                try!(self.reader.read(buf))
            } else {
                try!(self.reader.read(&mut buf[..self.left]))
            };
            self.digest.write(&buf[..read]);
            self.left -= read;
            Ok(read)
        }
    }

    #[cfg(test)]
    mod test {
        use std::io::{Read, Write};
        use std::path::Path;
        use std::sync::{Arc, RwLock};
        use std::fs::OpenOptions;
        use tempdir::TempDir;
        use util::HandyRwLock;
        use super::super::{SnapKey, Snapshot};
        use super::Snap;

        #[test]
        fn test_snap_file() {
            let dir = TempDir::new("test-snap-file").unwrap();
            let str = dir.path().to_str().unwrap().to_owned() + "/snap1";
            let path = Path::new(&str);
            assert!(!path.exists());
            let key = SnapKey::new(1, 1, 1);
            let test_data = b"test_data";
            let exp_len = (test_data.len() + super::CRC32_BYTES_COUNT) as u64;
            let size_track = Arc::new(RwLock::new(0));
            let mut f1 = Snap::new_for_writing(&path, size_track.clone(), true, &key).unwrap();
            let mut f2 = Snap::new_for_writing(&path, size_track.clone(), false, &key).unwrap();
            assert!(!f1.exists());
            assert!(Path::new(&f1.tmp_file.as_ref().unwrap().1).exists());
            assert!(!f2.exists());
            assert!(Path::new(&f2.tmp_file.as_ref().unwrap().1).exists());
            f1.write_all(test_data).unwrap();
            f2.write_all(test_data).unwrap();
            assert!(!f1.exists());
            assert!(Path::new(&f1.tmp_file.as_ref().unwrap().1).exists());
            assert!(!f2.exists());
            assert!(Path::new(&f2.tmp_file.as_ref().unwrap().1).exists());
            let key2 = SnapKey::new(2, 1, 1);
            let mut f3 = Snap::new_for_writing(&path, size_track.clone(), true, &key2).unwrap();
            let mut f4 = Snap::new_for_writing(&path, size_track.clone(), false, &key2).unwrap();
            f3.write_all(test_data).unwrap();
            f4.write_all(test_data).unwrap();
            assert_eq!(*size_track.rl(), 0);
            f3.save_with_checksum().unwrap();
            f4.save_with_checksum().unwrap();
            assert!(f3.exists());
            assert!(f4.exists());
            assert_eq!(*size_track.rl(), exp_len * 2);
        }

        #[test]
        fn test_snap_validate() {
            let path = TempDir::new("test-snap-mgr").unwrap();
            let path_str = path.path().to_str().unwrap();

            let key1 = SnapKey::new(1, 1, 1);
            let size_track = Arc::new(RwLock::new(0));
            let mut f1 = Snap::new_for_writing(path_str, size_track.clone(), false, &key1).unwrap();
            f1.write_all(b"testdata").unwrap();
            f1.save_with_checksum().unwrap();
            let mut reader = f1.get_validation_reader().unwrap();
            reader.validate().unwrap();

            // read partially should not affect validation.
            reader = f1.get_validation_reader().unwrap();
            reader.read_exact(&mut [0, 0]).unwrap();
            reader.validate().unwrap();

            // read fully should not affect validation.
            reader = f1.get_validation_reader().unwrap();
            while reader.read(&mut [0, 0]).unwrap() != 0 {}
            reader.validate().unwrap();

            let mut f = OpenOptions::new().write(true).open(f1.path()).unwrap();
            f.write_all(b"et").unwrap();
            reader = f1.get_validation_reader().unwrap();
            reader.validate().unwrap_err();
        }
    }
}

mod v2 {
    use std::io::{self, Read, Write, ErrorKind};
    use std::fs::{self, File, OpenOptions, Metadata};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, RwLock};
    use std::str;
    use std::time::Instant;
    use crc::crc32::{self, Digest, Hasher32};
    use protobuf::{Message, RepeatedField};
    use kvproto::metapb::Region;
    use kvproto::raft_serverpb::{SnapshotCFFile, SnapshotMeta, RaftSnapshotData};
    use rocksdb::{Writable, WriteBatch, EnvOptions, SstFileWriter, IngestExternalFileOptions};
    use storage::{CfName, CF_DEFAULT, CF_LOCK, CF_WRITE};
    use util::{HandyRwLock, rocksdb, duration_to_sec};
    use util::codec::bytes::{BytesEncoder, CompactBytesDecoder};
    use raftstore::Result as RaftStoreResult;

    use super::super::engine::{Snapshot as DbSnapshot, Iterable};
    use super::super::keys::{self, enc_start_key, enc_end_key};
    use super::super::util;
    use super::super::metrics::{SNAPSHOT_CF_KV_COUNT, SNAPSHOT_BUILD_TIME_HISTOGRAM};
    use super::{SNAP_GEN_PREFIX, SNAP_REV_PREFIX, TMP_FILE_SUFFIX, SST_FILE_SUFFIX, Result,
                SnapKey, Snapshot, BuildContext, ApplyOptions, check_abort, need_to_pack};

    pub const SNAPSHOT_VERSION: u64 = 2;
    const META_FILE_SUFFIX: &'static str = ".meta";
    const DIGEST_BUFFER_SIZE: usize = 10240;
    const SNAPSHOT_CFS: &'static [CfName] = &[CF_DEFAULT, CF_LOCK, CF_WRITE];

    fn get_file_size(path: &str) -> io::Result<u64> {
        let meta = try!(fs::metadata(path));
        Ok(meta.len())
    }

    fn file_exists(file: &str) -> bool {
        let path = Path::new(file);
        path.exists() && path.is_file()
    }

    fn delete_file(file: &str) -> bool {
        if !file_exists(file) {
            return true;
        }
        if let Err(e) = fs::remove_file(file) {
            warn!("failed to delete file {}: {:?}", file, e);
            return false;
        }
        true
    }

    fn calc_checksum(p: &str) -> io::Result<u32> {
        let mut digest = Digest::new(crc32::IEEE);
        let mut f = try!(OpenOptions::new().read(true).open(&p));
        let mut buf = vec![0; DIGEST_BUFFER_SIZE];
        loop {
            match f.read(&mut buf[..]) {
                Ok(0) => {
                    return Ok(digest.sum32());
                }
                Ok(n) => {
                    digest.write(&buf[..n]);
                }
                Err(ref e) if e.kind() == ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
    }

    fn get_snapshot_meta(cf_files: &[CfFile]) -> RaftStoreResult<(u64, SnapshotMeta)> {
        let mut total_size = 0;
        let mut meta = Vec::with_capacity(cf_files.len());
        for cf_file in cf_files {
            if SNAPSHOT_CFS.iter().find(|&cf| cf_file.cf == *cf).is_none() {
                return Err(box_err!("failed to encode invalid snapshot cf {}", cf_file.cf));
            }

            total_size += cf_file.size;

            let mut cf_file_meta = SnapshotCFFile::new();
            cf_file_meta.set_cf(cf_file.cf.to_owned());
            cf_file_meta.set_size(cf_file.size);
            cf_file_meta.set_checksum(cf_file.checksum);
            meta.push(cf_file_meta);
        }
        let mut snapshot_meta = SnapshotMeta::new();
        snapshot_meta.set_cf_files(RepeatedField::from_vec(meta));
        Ok((total_size, snapshot_meta))
    }

    fn check_snapshot_meta(snapshot_meta: &SnapshotMeta) -> RaftStoreResult<Vec<CfName>> {
        let mut res = Vec::with_capacity(SNAPSHOT_CFS.len());
        for cf_file_meta in snapshot_meta.get_cf_files() {
            let found = SNAPSHOT_CFS.iter().find(|&s| cf_file_meta.get_cf() == *s);
            if found.is_none() {
                return Err(box_err!("failed to decode invalid snapshot cf {}",
                                    cf_file_meta.get_cf()));
            }
            let cf_name = found.unwrap();
            if res.iter().any(|cf| cf_name == cf) {
                return Err(box_err!("failed to decode duplicated snapshot cf {}", cf_name));
            }
            res.push(*cf_name);
        }
        if res.len() != SNAPSHOT_CFS.len() {
            return Err(box_err!("invalid number of snapshot meta cf file: {}, expect: {}",
                                res.len(),
                                SNAPSHOT_CFS.len()));
        }
        Ok(res)
    }

    #[derive(Default)]
    struct CfFile {
        pub cf: CfName,
        pub path: String,
        pub size: u64,

        // for building snapshot
        pub sst_writer: Option<SstFileWriter>,
        pub kv_count: u64,
        // for sending/receiving snapshot
        pub file: Option<File>,

        // for writing snapshot
        pub tmp_path: String,
        pub written_size: u64,

        pub checksum: u32,
    }

    #[derive(Default)]
    struct MetaFile {
        pub meta: SnapshotMeta,
        pub path: String,
        pub file: Option<File>,

        // for writing snapshot
        pub tmp_path: String,
    }

    pub struct Snap {
        display_path: String,
        cf_files: Vec<CfFile>,
        cf_index: usize,
        meta_file: MetaFile,
        size_track: Arc<RwLock<u64>>,
    }

    impl Snap {
        fn new<T: Into<PathBuf>>(dir: T,
                                 key: &SnapKey,
                                 size_track: Arc<RwLock<u64>>,
                                 cfs: &[CfName],
                                 sending: bool)
                                 -> io::Result<Snap> {
            let dir_path = dir.into();
            if !dir_path.exists() {
                try!(fs::create_dir_all(dir_path.as_path()));
            }
            let snap_prefix = if sending {
                SNAP_GEN_PREFIX
            } else {
                SNAP_REV_PREFIX
            };
            let prefix = format!("{}_{}", snap_prefix, key);
            let display_path = Snap::get_display_path(&dir_path, &prefix);
            let mut cf_files = Vec::with_capacity(SNAPSHOT_CFS.len());
            for cf in cfs {
                let filename = format!("{}_{}{}", prefix, cf, SST_FILE_SUFFIX);
                let path = dir_path.join(filename).as_path().to_str().unwrap().to_owned();
                let tmp_path = format!("{}{}", path, TMP_FILE_SUFFIX);
                let cf_file = CfFile {
                    cf: cf,
                    path: path,
                    tmp_path: tmp_path,
                    ..Default::default()
                };
                cf_files.push(cf_file);
            }
            let meta_filename = format!("{}{}", prefix, META_FILE_SUFFIX);
            let meta_path = dir_path.join(meta_filename).as_path().to_str().unwrap().to_owned();
            let meta_tmp_path = format!("{}{}", meta_path, TMP_FILE_SUFFIX);
            let meta_file = MetaFile {
                path: meta_path,
                tmp_path: meta_tmp_path,
                ..Default::default()
            };
            let s = Snap {
                display_path: display_path,
                cf_files: cf_files,
                cf_index: 0,
                meta_file: meta_file,
                size_track: size_track,
            };
            Ok(s)
        }

        pub fn new_for_building<T: Into<PathBuf>>(dir: T,
                                                  key: &SnapKey,
                                                  snap: &DbSnapshot,
                                                  size_track: Arc<RwLock<u64>>)
                                                  -> RaftStoreResult<Snap> {
            let mut s = try!(Snap::new(dir, key, size_track, SNAPSHOT_CFS, true));
            try!(s.init_for_building(snap));
            Ok(s)
        }

        pub fn new_for_sending<T: Into<PathBuf>>(dir: T,
                                                 key: &SnapKey,
                                                 size_track: Arc<RwLock<u64>>)
                                                 -> RaftStoreResult<Snap> {
            let mut s = try!(Snap::new(dir, key, size_track, SNAPSHOT_CFS, true));
            try!(s.init_for_sending());
            Ok(s)
        }

        pub fn new_for_receiving<T: Into<PathBuf>>(dir: T,
                                                   key: &SnapKey,
                                                   snapshot_meta: SnapshotMeta,
                                                   size_track: Arc<RwLock<u64>>)
                                                   -> RaftStoreResult<Snap> {
            let cfs = try!(check_snapshot_meta(&snapshot_meta));
            let mut s = try!(Snap::new(dir, key, size_track, &cfs[..], false));
            try!(s.init_for_receiving(snapshot_meta));
            Ok(s)
        }

        pub fn new_for_applying<T: Into<PathBuf>>(dir: T,
                                                  key: &SnapKey,
                                                  size_track: Arc<RwLock<u64>>)
                                                  -> RaftStoreResult<Snap> {
            let mut s = try!(Snap::new(dir, key, size_track, SNAPSHOT_CFS, false));
            try!(s.init_for_applying());
            Ok(s)
        }

        fn init_for_building(&mut self, snap: &DbSnapshot) -> RaftStoreResult<()> {
            if self.exists() {
                return Ok(());
            }
            let env_opt = EnvOptions::new();
            for cf_file in &mut self.cf_files {
                if cf_file.cf == CF_LOCK {
                    let f = try!(OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&cf_file.tmp_path));
                    cf_file.file = Some(f);
                } else {
                    // initialize sst file writer
                    let handle = try!(snap.cf_handle(&cf_file.cf));
                    let io_options = snap.get_db().get_options_cf(handle);
                    let mut writer = SstFileWriter::new(&env_opt, &io_options, handle);
                    box_try!(writer.open(&cf_file.tmp_path));
                    cf_file.sst_writer = Some(writer);
                }
            }
            let file = try!(OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&self.meta_file.tmp_path));
            self.meta_file.file = Some(file);
            Ok(())
        }

        fn init_for_sending(&mut self) -> RaftStoreResult<()> {
            if !self.exists() {
                return Err(box_err!("snapshot file {} not exist", self.path()));
            }
            for cf_file in &mut self.cf_files {
                // initialize cf file size and reader
                let size = try!(get_file_size(&cf_file.path));
                let file = try!(File::open(&cf_file.path));
                cf_file.size = size;
                cf_file.file = Some(file);
            }
            self.load_snapshot_metadata()
        }

        fn init_for_receiving(&mut self, snapshot_meta: SnapshotMeta) -> RaftStoreResult<()> {
            if self.exists() {
                return Ok(());
            }
            try!(self.set_snapshot_meta(snapshot_meta));
            for cf_file in &mut self.cf_files {
                let f =
                    try!(OpenOptions::new().write(true).create_new(true).open(&cf_file.tmp_path));
                cf_file.file = Some(f);
            }
            let f = try!(OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&self.meta_file.tmp_path));
            self.meta_file.file = Some(f);
            Ok(())
        }

        fn init_for_applying(&mut self) -> RaftStoreResult<()> {
            if !self.exists() {
                return Err(box_err!("snapshot file {} not exists", self.path()));
            }
            self.load_snapshot_metadata()
        }

        fn read_snapshot_meta(&mut self) -> RaftStoreResult<SnapshotMeta> {
            let size = try!(get_file_size(&self.meta_file.path));
            let mut file = try!(File::open(&self.meta_file.path));
            let mut buf = Vec::with_capacity(size as usize);
            try!(file.read_to_end(&mut buf));
            let mut snapshot_meta = SnapshotMeta::new();
            try!(snapshot_meta.merge_from_bytes(&buf));
            Ok(snapshot_meta)
        }

        fn set_snapshot_meta(&mut self, snapshot_meta: SnapshotMeta) -> RaftStoreResult<()> {
            for cf_file in &mut self.cf_files {
                let found = snapshot_meta.get_cf_files().iter().find(|x| x.get_cf() == cf_file.cf);
                if found.is_none() {
                    return Err(box_err!("cf {} doesn't exist in snapshot meta", cf_file.cf));
                }
                let meta = found.unwrap();
                cf_file.size = meta.get_size();
                cf_file.checksum = meta.get_checksum();
            }
            self.meta_file.meta = snapshot_meta;
            Ok(())
        }

        fn load_snapshot_metadata(&mut self) -> RaftStoreResult<()> {
            let snapshot_meta = try!(self.read_snapshot_meta());
            try!(self.set_snapshot_meta(snapshot_meta));
            Ok(())
        }

        fn get_display_path(dir_path: &PathBuf, prefix: &str) -> String {
            let mut cf_names = SNAPSHOT_CFS.iter().fold(String::from(""), |mut acc, x| {
                if acc.is_empty() {
                    acc += "(";
                    acc += x;
                } else {
                    acc += "|";
                    acc += x;
                }
                acc
            });
            cf_names += ")";
            format!("{}/{}_{}{}",
                    dir_path.as_path().to_str().unwrap().to_owned(),
                    prefix,
                    cf_names,
                    SST_FILE_SUFFIX)
        }

        fn try_delete(&self, to_be_dropped: bool) {
            debug!("deleting {}", self.path());
            let exists = if to_be_dropped { self.exists() } else { true };
            let mut total_size = 0;
            for cf_file in &self.cf_files {
                delete_file(&cf_file.tmp_path);
                delete_file(&cf_file.path);
                if exists {
                    total_size += cf_file.size;
                }
            }
            delete_file(&self.meta_file.tmp_path);
            delete_file(&self.meta_file.path);
            if exists {
                let mut size_track = self.size_track.wl();
                *size_track = size_track.saturating_sub(total_size);
            }
        }

        fn validate(&self) -> RaftStoreResult<()> {
            for cf_file in &self.cf_files {
                let size = try!(get_file_size(&cf_file.path));
                if size != cf_file.size {
                    return Err(box_err!("invalid snapshot file size {} for cf {}, expected {}",
                                        size,
                                        cf_file.cf,
                                        cf_file.size));
                }
                let checksum = try!(calc_checksum(&cf_file.path));
                if checksum != cf_file.checksum {
                    return Err(box_err!("invalid snapshot file checksum {} for cf {}, expected \
                                         {}",
                                        checksum,
                                        cf_file.cf,
                                        cf_file.checksum));
                }
            }
            Ok(())
        }

        fn switch_to_cf_file(&mut self, cf: &str) -> io::Result<()> {
            match self.cf_files.iter().position(|x| x.cf == cf) {
                Some(index) => {
                    self.cf_index = index;
                    Ok(())
                }
                None => Err(io::Error::new(ErrorKind::Other, format!("fail to find cf {}", cf))),
            }
        }

        fn add_kv(&mut self, k: &[u8], v: &[u8]) -> io::Result<()> {
            let mut cf_file = &mut self.cf_files[self.cf_index];
            let mut writer = cf_file.sst_writer.as_mut().unwrap();
            if let Err(e) = writer.add(k, v) {
                return Err(io::Error::new(ErrorKind::Other, e));
            }
            cf_file.kv_count += 1;
            Ok(())
        }

        fn save_cf_files(&mut self) -> io::Result<()> {
            for cf_file in &mut self.cf_files {
                if cf_file.cf == CF_LOCK {
                    let _ = cf_file.file.take();
                } else if cf_file.kv_count == 0 {
                    let _ = cf_file.sst_writer.take().unwrap();
                } else {
                    let mut writer = cf_file.sst_writer.take().unwrap();
                    if let Err(e) = writer.finish() {
                        return Err(io::Error::new(ErrorKind::Other, e));
                    }
                }
                try!(fs::rename(&cf_file.tmp_path, &cf_file.path));
                cf_file.size = try!(get_file_size(&cf_file.path));
                cf_file.checksum = try!(calc_checksum(&cf_file.path));
            }
            Ok(())
        }

        fn save_meta_file(&mut self) -> RaftStoreResult<()> {
            let mut v = vec![];
            box_try!(self.meta_file.meta.write_to_vec(&mut v));
            {
                let mut f = self.meta_file.file.take().unwrap();
                try!(f.write_all(&v[..]));
                try!(f.flush());
            }
            try!(fs::rename(&self.meta_file.tmp_path, &self.meta_file.path));
            Ok(())
        }

        fn do_build(&mut self,
                    snap: &DbSnapshot,
                    region: &Region,
                    context: &mut BuildContext)
                    -> RaftStoreResult<()> {
            if self.exists() {
                match self.validate() {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        warn!("[region {}] file {} is invalid, will rebuild: {:?}",
                              region.get_id(),
                              self.path(),
                              e);
                        self.delete();
                        try!(self.init_for_building(snap));
                    }
                }
            }

            let mut snap_key_count = 0;
            let (begin_key, end_key) = (enc_start_key(region), enc_end_key(region));
            for cf in snap.cf_names() {
                if !need_to_pack(cf) {
                    continue;
                }
                try!(self.switch_to_cf_file(cf));
                let mut cf_key_count = 0;
                if cf == CF_LOCK {
                    let file = self.cf_files[self.cf_index].file.as_mut().unwrap();
                    try!(snap.scan_cf(cf,
                                      &begin_key,
                                      &end_key,
                                      false,
                                      &mut |key, value| {
                                          cf_key_count += 1;
                                          try!(file.encode_compact_bytes(key));
                                          try!(file.encode_compact_bytes(value));
                                          Ok(true)
                                      }));
                    // use an empty byte array to indicate that cf reaches an end.
                    try!(file.encode_compact_bytes(b""));
                } else {
                    try!(snap.scan_cf(cf,
                                      &begin_key,
                                      &end_key,
                                      false,
                                      &mut |key, value| {
                                          cf_key_count += 1;
                                          try!(self.add_kv(key, value));
                                          Ok(true)
                                      }));
                }
                snap_key_count += cf_key_count;
                SNAPSHOT_CF_KV_COUNT.with_label_values(&[cf]).observe(cf_key_count as f64);
                info!("[region {} scan snapshot {}, cf {}, key count {}]",
                      region.get_id(),
                      self.path(),
                      cf,
                      cf_key_count)
            }
            try!(self.save_cf_files());
            context.snapshot_kv_count = snap_key_count;

            Ok(())
        }
    }

    impl Snapshot for Snap {
        fn build(&mut self,
                 snap: &DbSnapshot,
                 region: &Region,
                 snap_data: &mut RaftSnapshotData,
                 context: &mut BuildContext)
                 -> RaftStoreResult<()> {
            let t = Instant::now();
            try!(self.do_build(snap, region, context));
            // set snapshot meta data
            let (total_size, snapshot_meta) = try!(get_snapshot_meta(&self.cf_files[..]));
            snap_data.set_file_size(total_size);
            snap_data.set_version(SNAPSHOT_VERSION);
            snap_data.set_meta(snapshot_meta.clone());
            // save snapshot meta file to meta file
            self.meta_file.meta = snapshot_meta;
            try!(self.save_meta_file());
            // add size
            let mut size_track = self.size_track.wl();
            *size_track = size_track.saturating_add(total_size);
            context.snapshot_size = total_size;
            SNAPSHOT_BUILD_TIME_HISTOGRAM.observe(duration_to_sec(t.elapsed()) as f64);
            info!("[region {}] scan snapshot {}, size {}, key count {}, takes {:?}",
                  region.get_id(),
                  self.path(),
                  total_size,
                  context.snapshot_kv_count,
                  t.elapsed());

            Ok(())
        }

        fn path(&self) -> &str {
            &self.display_path
        }

        fn exists(&self) -> bool {
            self.cf_files.iter().all(|cf_file| file_exists(&cf_file.path)) &&
            file_exists(&self.meta_file.path)
        }

        fn delete(&self) {
            self.try_delete(false);
        }

        fn meta(&self) -> io::Result<Metadata> {
            fs::metadata(&self.meta_file.path)
        }

        fn total_size(&self) -> io::Result<u64> {
            let mut total_size = 0;
            for cf_file in &self.cf_files {
                total_size += cf_file.size;
            }
            Ok(total_size)
        }

        fn save(&mut self) -> io::Result<()> {
            debug!("saving to {}", self.path());
            let mut total_size = 0;
            for cf_file in &mut self.cf_files {
                // Check each cf file has been fully written, and the checksum matches.
                {
                    let mut file = cf_file.file.take().unwrap();
                    try!(file.flush());
                }
                let size = try!(get_file_size(&cf_file.tmp_path));
                if size != cf_file.size {
                    return Err(io::Error::new(ErrorKind::Other,
                                              format!("snapshot file {} for cf {} size \
                                                       mismatches, real size {}, expected size \
                                                       {}",
                                                      cf_file.path,
                                                      cf_file.cf,
                                                      size,
                                                      cf_file.size)));
                }
                let checksum = try!(calc_checksum(&cf_file.tmp_path));
                if checksum != cf_file.checksum {
                    return Err(io::Error::new(ErrorKind::Other,
                                              format!("snapshot file {} for cf {} checksum \
                                                       mismatches, real checksum {}, expected \
                                                       checksum {}",
                                                      cf_file.path,
                                                      cf_file.cf,
                                                      checksum,
                                                      cf_file.checksum)));
                }

                try!(fs::rename(&cf_file.tmp_path, &cf_file.path));
                total_size += cf_file.size;
            }
            // write meta file
            let mut v = vec![];
            try!(self.meta_file.meta.write_to_vec(&mut v));
            try!(self.meta_file.file.take().unwrap().write_all(&v[..]));
            try!(fs::rename(&self.meta_file.tmp_path, &self.meta_file.path));
            let mut size_track = self.size_track.wl();
            *size_track = size_track.saturating_add(total_size);
            Ok(())
        }

        fn apply(&mut self, options: ApplyOptions) -> Result<()> {
            for cf_file in &mut self.cf_files {
                try!(check_abort(&options.abort));
                let cf_handle = box_try!(rocksdb::get_cf_handle(&options.db, &cf_file.cf));
                if cf_file.cf == CF_LOCK {
                    let mut file = box_try!(File::open(&cf_file.path));
                    let mut wb = WriteBatch::new();
                    let mut batch_size = 0;
                    loop {
                        try!(check_abort(&options.abort));
                        let key = box_try!(file.decode_compact_bytes());
                        if key.is_empty() {
                            box_try!(options.db.write(wb));
                            break;
                        }
                        box_try!(util::check_key_in_region(keys::origin_key(&key),
                                                           &options.region));
                        batch_size += key.len();
                        let value = box_try!(file.decode_compact_bytes());
                        batch_size += value.len();
                        box_try!(wb.put_cf(cf_handle, &key, &value));
                        if batch_size >= options.write_batch_size {
                            box_try!(options.db.write(wb));
                            wb = WriteBatch::new();
                            batch_size = 0;
                        }
                    }
                } else {
                    if cf_file.size == 0 {
                        // Skip ingesting empty sst file since that would cause an error
                        // "Corruption: file is too short to be an sstable"
                        continue;
                    }

                    let ingest_opt = IngestExternalFileOptions::new().move_files(true);
                    box_try!(options.db
                        .ingest_external_file_cf(cf_handle, &ingest_opt, &[&cf_file.path]));
                }
            }
            Ok(())
        }
    }

    impl Read for Snap {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.len() == 0 {
                return Ok(0);
            }
            while self.cf_index < self.cf_files.len() {
                match self.cf_files[self.cf_index].file.as_mut().unwrap().read(buf) {
                    Ok(0) => {
                        // EOF. Switch to next file.
                        self.cf_index += 1;
                    }
                    Ok(n) => {
                        return Ok(n);
                    }
                    e => return e,
                }
            }
            Ok(0)
        }
    }

    impl Write for Snap {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if buf.len() == 0 {
                return Ok(0);
            }
            let mut next_buf = buf;
            while self.cf_index < self.cf_files.len() {
                let cf_file = &mut self.cf_files[self.cf_index];
                let left = (cf_file.size - cf_file.written_size) as usize;
                if left == 0 {
                    self.cf_index += 1;
                    continue;
                }
                let mut file = cf_file.file.as_mut().unwrap();
                if next_buf.len() > left {
                    try!(file.write_all(&next_buf[0..left]));
                    cf_file.written_size += left as u64;
                    self.cf_index += 1;
                    next_buf = &next_buf[left..];
                } else {
                    try!(file.write_all(next_buf));
                    cf_file.written_size += next_buf.len() as u64;
                    return Ok(buf.len());
                }
            }
            let n = buf.len() - next_buf.len();
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            if let Some(cf_file) = self.cf_files.get_mut(self.cf_index) {
                let file = cf_file.file.as_mut().unwrap();
                try!(file.flush());
            }
            Ok(())
        }
    }

    impl Drop for Snap {
        fn drop(&mut self) {
            if self.cf_files.iter().any(|cf_file| file_exists(&cf_file.tmp_path)) ||
               file_exists(&self.meta_file.tmp_path) {
                self.try_delete(true)
            }
        }
    }

    #[cfg(test)]
    pub mod test {
        use std::io;
        use std::sync::{Arc, RwLock};
        use std::sync::atomic::AtomicUsize;
        use tempdir::TempDir;
        use kvproto::metapb::{Peer, Region};
        use kvproto::raft_serverpb::RaftSnapshotData;
        use rocksdb::DB;

        use storage::ALL_CFS;
        use util::{rocksdb, HandyRwLock};
        use raftstore::Result;
        use raftstore::store::keys;
        use raftstore::store::engine::{Snapshot as DbSnapshot, Mutable, Peekable, Iterable};
        use raftstore::store::peer_storage::JOB_STATUS_RUNNING;
        use super::super::{SNAP_GEN_PREFIX, need_to_pack, SnapKey, Snapshot};
        use super::{Snap, BuildContext, ApplyOptions};

        const TEST_STORE_ID: u64 = 1;
        const TEST_KEY: &[u8] = b"akey";

        pub fn get_test_empty_db(path: &TempDir) -> Result<Arc<DB>> {
            let p = path.path().to_str().unwrap();
            let db = try!(rocksdb::new_engine(p, ALL_CFS));
            Ok(Arc::new(db))
        }

        pub fn get_test_db(path: &TempDir) -> Result<Arc<DB>> {
            let p = path.path().to_str().unwrap();
            let db = try!(rocksdb::new_engine(p, ALL_CFS));
            let key = keys::data_key(TEST_KEY);
            // write some data into each cf
            for (i, cf) in ALL_CFS.into_iter().enumerate() {
                let handle = try!(rocksdb::get_cf_handle(&db, cf));
                let mut p = Peer::new();
                p.set_store_id(TEST_STORE_ID);
                p.set_id((i + 1) as u64);
                try!(db.put_msg_cf(handle, &key[..], &p));
            }
            Ok(Arc::new(db))
        }

        pub fn get_kv_count(snap: &DbSnapshot) -> usize {
            let mut kv_count = 0;
            for cf in snap.cf_names() {
                if !need_to_pack(cf) {
                    continue;
                }
                snap.scan_cf(cf,
                             &keys::data_key(b"a"),
                             &keys::data_key(b"z"),
                             false,
                             &mut |_, _| {
                                 kv_count += 1;
                                 Ok(true)
                             })
                    .unwrap();
            }
            kv_count
        }

        pub fn get_test_region(region_id: u64, store_id: u64, peer_id: u64) -> Region {
            let mut peer = Peer::new();
            peer.set_store_id(store_id);
            peer.set_id(peer_id);
            let mut region = Region::new();
            region.set_id(region_id);
            region.set_start_key(b"a".to_vec());
            region.set_end_key(b"z".to_vec());
            region.mut_region_epoch().set_version(1);
            region.mut_region_epoch().set_conf_ver(1);
            region.mut_peers().push(peer.clone());
            region
        }

        pub fn assert_eq_db(expected_db: Arc<DB>, db: &DB) {
            let key = keys::data_key(TEST_KEY);
            for cf in ALL_CFS {
                if !need_to_pack(cf) {
                    continue;
                }
                let p1: Option<Peer> = expected_db.get_msg_cf(cf, &key[..]).unwrap();
                if p1.is_some() {
                    let p2: Option<Peer> = db.get_msg_cf(cf, &key[..]).unwrap();
                    if !p2.is_some() {
                        panic!("cf {}: expect key {:?} has value", cf, key);
                    }
                    let p1 = p1.unwrap();
                    let p2 = p2.unwrap();
                    if p2 != p1 {
                        panic!("cf {}: key {:?}, value {:?}, expected {:?}",
                               cf,
                               key,
                               p2,
                               p1);
                    }
                }
            }
        }

        #[test]
        fn test_get_and_check_snapshot_meta() {
            let mut cf_file = Vec::with_capacity(super::SNAPSHOT_CFS.len());
            for (i, cf) in super::SNAPSHOT_CFS.iter().enumerate() {
                let f = super::CfFile {
                    cf: cf,
                    size: 100 * (i + 1) as u64,
                    checksum: 1000 * (i + 1) as u32,
                    ..Default::default()
                };
                cf_file.push(f);
            }
            let (_, meta) = super::get_snapshot_meta(&cf_file).unwrap();
            let _ = super::check_snapshot_meta(&meta).unwrap();
            for (i, cf_file_meta) in meta.get_cf_files().iter().enumerate() {
                if cf_file_meta.get_cf() != cf_file[i].cf {
                    panic!("{}: expect cf {}, got {}",
                           i,
                           cf_file[i].cf,
                           cf_file_meta.get_cf());
                }
                if cf_file_meta.get_size() != cf_file[i].size {
                    panic!("{}: expect cf size {}, got {}",
                           i,
                           cf_file[i].size,
                           cf_file_meta.get_size());
                }
                if cf_file_meta.get_checksum() != cf_file[i].checksum {
                    panic!("{}: expect cf checksum {}, got {}",
                           i,
                           cf_file[i].checksum,
                           cf_file_meta.get_checksum());
                }
            }
        }

        #[test]
        fn test_display_path() {
            let dir = TempDir::new("test-display-path").unwrap();
            let key = SnapKey::new(1, 1, 1);
            let prefix = format!("{}_{}", SNAP_GEN_PREFIX, key);
            let display_path = Snap::get_display_path(&dir.into_path(), &prefix);
            assert!(display_path != "");
        }

        #[test]
        fn test_empty_snap_file() {
            test_snap_file(get_test_empty_db);
        }

        #[test]
        fn test_non_empty_snap_file() {
            test_snap_file(get_test_db);
        }

        fn test_snap_file(get_db: fn(p: &TempDir) -> Result<Arc<DB>>) {
            let region_id = 1;
            let region = get_test_region(region_id, 1, 1);
            let src_db_dir = TempDir::new("test-snap-file-db-src").unwrap();
            let db = get_db(&src_db_dir).unwrap();
            let snapshot = DbSnapshot::new(db.clone());

            let src_dir = TempDir::new("test-snap-file-src").unwrap();
            let key = SnapKey::new(region_id, 1, 1);
            let size_track = Arc::new(RwLock::new(0));
            let mut s1 =
                Snap::new_for_building(src_dir.path(), &key, &snapshot, size_track.clone())
                    .unwrap();
            // Ensure that this snapshot file doesn't exist before being built.
            assert!(!s1.exists());
            assert_eq!(*size_track.rl(), 0);

            let mut snap_data = RaftSnapshotData::new();
            snap_data.set_region(region.clone());
            let mut context = BuildContext::new();
            s1.build(&snapshot, &region, &mut snap_data, &mut context).unwrap();

            // Ensure that this snapshot file does exist after being built.
            assert!(s1.exists());
            let total_size = s1.total_size().unwrap();
            // Ensure the `size_track` is modified correctly.
            let size = *size_track.rl();
            assert_eq!(size, total_size);
            assert_eq!(context.snapshot_size as u64, size);
            assert_eq!(context.snapshot_kv_count, get_kv_count(&snapshot));

            // Ensure this snapshot could be read for sending.
            let mut s2 = Snap::new_for_sending(src_dir.path(), &key, size_track.clone()).unwrap();
            assert!(s2.exists());

            // TODO check meta data correct.
            let _ = s2.meta().unwrap();

            let dst_dir = TempDir::new("test-snap-file-dst").unwrap();

            let mut s3 = Snap::new_for_receiving(dst_dir.path(),
                                                 &key,
                                                 snap_data.take_meta(),
                                                 size_track.clone())
                .unwrap();
            assert!(!s3.exists());

            // Ensure snapshot data could be read out of `s2`, and write into `s3`.
            let copy_size = io::copy(&mut s2, &mut s3).unwrap();
            assert_eq!(copy_size, size);
            assert!(!s3.exists());
            s3.save().unwrap();
            assert!(s3.exists());

            // Ensure the tracked size is handled correctly after receiving a snapshot.
            assert_eq!(*size_track.rl(), size * 2);

            // Ensure `delete()` works to delete the source snapshot.
            s2.delete();
            assert!(!s2.exists());
            assert!(!s1.exists());
            assert_eq!(*size_track.rl(), size);

            // Ensure a snapshot could be applied to DB.
            let mut s4 = Snap::new_for_applying(dst_dir.path(), &key, size_track.clone()).unwrap();
            assert!(s4.exists());

            let dst_db_dir = TempDir::new("test-snap-file-db-dst").unwrap();
            let dst_db = Arc::new(rocksdb::new_engine(dst_db_dir.path().to_str().unwrap(),
                                                      ALL_CFS)
                .unwrap());
            let options = ApplyOptions {
                db: dst_db.clone(),
                region: region.clone(),
                abort: Arc::new(AtomicUsize::new(JOB_STATUS_RUNNING)),
                write_batch_size: 10 * 1024 * 1024,
            };
            s4.apply(options).unwrap();

            // Ensure `delete()` works to delete the dest snapshot.
            s4.delete();
            assert!(!s4.exists());
            assert!(!s3.exists());
            assert_eq!(*size_track.rl(), 0);

            // Verify the data is correct after applying snapshot.
            assert_eq_db(db, dst_db.as_ref());
        }
    }
}

#[derive(PartialEq, Debug)]
pub enum SnapEntry {
    Generating = 1,
    Sending = 2,
    Receiving = 3,
    Applying = 4,
}

/// `SnapStats` is for snapshot statistics.
pub struct SnapStats {
    pub sending_count: usize,
    pub receiving_count: usize,
}

/// `SnapManagerCore` trace all current processing snapshots.
pub struct SnapManagerCore {
    // directory to store snapfile.
    base: String,
    registry: HashMap<SnapKey, Vec<SnapEntry>>,
    ch: Option<SendCh<Msg>>,
    use_sst_file_snapshot: bool,
    snap_size: Arc<RwLock<u64>>,
}

impl SnapManagerCore {
    pub fn new<T: Into<String>>(path: T,
                                ch: Option<SendCh<Msg>>,
                                use_sst_file_snapshot: bool)
                                -> SnapManagerCore {
        SnapManagerCore {
            base: path.into(),
            registry: map![],
            ch: ch,
            use_sst_file_snapshot: use_sst_file_snapshot,
            snap_size: Arc::new(RwLock::new(0)),
        }
    }

    pub fn init(&self) -> io::Result<()> {
        let path = Path::new(&self.base);
        if !path.exists() {
            try!(fs::create_dir_all(path));
            return Ok(());
        }
        if !path.is_dir() {
            return Err(io::Error::new(ErrorKind::Other,
                                      format!("{} should be a directory", path.display())));
        }
        let mut size = self.snap_size.wl();
        for f in try!(fs::read_dir(path)) {
            let p = try!(f);
            if try!(p.file_type()).is_file() {
                if let Some(s) = p.file_name().to_str() {
                    if s.ends_with(TMP_FILE_SUFFIX) {
                        try!(fs::remove_file(p.path()));
                    } else if s.ends_with(SNAP_FILE_SUFFIX) || s.ends_with(SST_FILE_SUFFIX) {
                        *size += try!(p.metadata()).len();
                    }
                }
            }
        }
        Ok(())
    }

    pub fn list_snap(&self) -> io::Result<Vec<(SnapKey, bool)>> {
        let path = Path::new(&self.base);
        let read_dir = try!(fs::read_dir(path));
        Ok(read_dir.filter_map(|p| {
                let p = match p {
                    Err(e) => {
                        error!("failed to list content of {}: {:?}", self.base, e);
                        return None;
                    }
                    Ok(p) => p,
                };
                match p.file_type() {
                    Ok(t) if t.is_file() => {}
                    _ => return None,
                }
                let file_name = p.file_name();
                let name = match file_name.to_str() {
                    None => return None,
                    Some(n) => n,
                };
                let is_sending = name.starts_with(SNAP_GEN_PREFIX);
                let numbers: Vec<u64> = name.split('.')
                    .next()
                    .map_or_else(|| vec![], |s| {
                        s.split('_')
                            .skip(1)
                            .filter_map(|s| s.parse().ok())
                            .collect()
                    });
                if numbers.len() != 3 {
                    error!("failed to parse snapkey from {}", name);
                    return None;
                }
                Some((SnapKey::new(numbers[0], numbers[1], numbers[2]), is_sending))
            })
            .collect())
    }

    #[inline]
    pub fn has_registered(&self, key: &SnapKey) -> bool {
        self.registry.contains_key(key)
    }

    pub fn get_snapshot_for_building(&self,
                                     key: &SnapKey,
                                     snap: &DbSnapshot)
                                     -> RaftStoreResult<Box<Snapshot>> {
        if self.use_sst_file_snapshot {
            let f = try!(v2::Snap::new_for_building(&self.base, key, snap, self.snap_size.clone()));
            Ok(Box::new(f))
        } else {
            let f = try!(v1::Snap::new_for_writing(&self.base, self.snap_size.clone(), true, key));
            Ok(Box::new(f))
        }
    }

    pub fn get_snapshot_for_sending(&self, key: &SnapKey) -> RaftStoreResult<Box<Snapshot>> {
        if let Ok(s) = v1::Snap::new_for_reading(&self.base, self.snap_size.clone(), true, key) {
            if s.exists() {
                return Ok(Box::new(s));
            }
        }
        let s = try!(v2::Snap::new_for_sending(&self.base, key, self.snap_size.clone()));
        Ok(Box::new(s))
    }

    pub fn get_snapshot_for_receiving(&self,
                                      key: &SnapKey,
                                      data: &[u8])
                                      -> RaftStoreResult<Box<Snapshot>> {
        let mut snapshot_data = RaftSnapshotData::new();
        try!(snapshot_data.merge_from_bytes(data));
        if snapshot_data.get_version() == v2::SNAPSHOT_VERSION {
            let f = try!(v2::Snap::new_for_receiving(&self.base,
                                                     key,
                                                     snapshot_data.take_meta(),
                                                     self.snap_size.clone()));
            Ok(Box::new(f))
        } else {
            let f = try!(v1::Snap::new_for_writing(&self.base, self.snap_size.clone(), false, key));
            Ok(Box::new(f))
        }
    }

    pub fn get_snapshot_for_applying(&self, key: &SnapKey) -> RaftStoreResult<Box<Snapshot>> {
        if let Ok(s) = v2::Snap::new_for_applying(&self.base, key, self.snap_size.clone()) {
            if s.exists() {
                return Ok(Box::new(s));
            }
        }
        let s = try!(v1::Snap::new_for_reading(&self.base, self.snap_size.clone(), false, key));
        Ok(Box::new(s))
    }

    /// Get the approximate size of snap file exists in snap directory.
    ///
    /// Return value is not guaranteed to be accurate.
    pub fn get_total_snap_size(&self) -> u64 {
        *self.snap_size.rl()
    }

    pub fn register(&mut self, key: SnapKey, entry: SnapEntry) {
        debug!("register [key: {}, entry: {:?}]", key, entry);
        match self.registry.entry(key) {
            Entry::Occupied(mut e) => {
                if e.get().contains(&entry) {
                    warn!("{} is registered more than 1 time!!!", e.key());
                    return;
                }
                e.get_mut().push(entry);
            }
            Entry::Vacant(e) => {
                e.insert(vec![entry]);
            }
        }

        self.notify_stats();
    }

    pub fn deregister(&mut self, key: &SnapKey, entry: &SnapEntry) {
        debug!("deregister [key: {}, entry: {:?}]", key, entry);
        let mut need_clean = false;
        let mut handled = false;
        if let Some(e) = self.registry.get_mut(key) {
            let last_len = e.len();
            e.retain(|e| e != entry);
            need_clean = e.is_empty();
            handled = last_len > e.len();
        }
        if need_clean {
            self.registry.remove(key);
        }
        if handled {
            self.notify_stats();
            return;
        }
        warn!("stale deregister key: {} {:?}", key, entry);
    }

    fn notify_stats(&self) {
        if let Some(ref ch) = self.ch {
            if let Err(e) = ch.try_send(Msg::SnapshotStats) {
                error!("notify snapshot stats failed {:?}", e)
            }
        }
    }

    pub fn stats(&self) -> SnapStats {
        // send_count, generating_count, receiving_count, applying_count
        let (mut sending_cnt, mut receiving_cnt) = (0, 0);
        for v in self.registry.values() {
            let (mut is_sending, mut is_receiving) = (false, false);
            for s in v {
                match *s {
                    SnapEntry::Sending | SnapEntry::Generating => is_sending = true,
                    SnapEntry::Receiving | SnapEntry::Applying => is_receiving = true,
                }
            }
            if is_sending {
                sending_cnt += 1;
            }
            if is_receiving {
                receiving_cnt += 1;
            }
        }

        SnapStats {
            sending_count: sending_cnt,
            receiving_count: receiving_cnt,
        }
    }
}

pub type SnapManager = Arc<RwLock<SnapManagerCore>>;

pub fn new_snap_mgr<T: Into<String>>(path: T,
                                     ch: Option<SendCh<Msg>>,
                                     use_sst_file_snapshot: bool)
                                     -> SnapManager {
    Arc::new(RwLock::new(SnapManagerCore::new(path, ch, use_sst_file_snapshot)))
}

#[cfg(test)]
mod test {
    use std::io;
    use std::sync::atomic::AtomicUsize;
    use std::fs::File;
    use std::io::Write;
    use std::sync::*;
    use tempdir::TempDir;
    use protobuf::Message;
    use kvproto::raft_serverpb::RaftSnapshotData;

    use storage::ALL_CFS;
    use util::{rocksdb, HandyRwLock};
    use super::super::peer_storage::JOB_STATUS_RUNNING;
    use super::super::engine::Snapshot as DbSnapshot;
    use super::{SnapKey, BuildContext, ApplyOptions, Snapshot, new_snap_mgr};
    use super::v1::{Snap as SnapV1, CRC32_BYTES_COUNT};
    use super::v2::{self, Snap as SnapV2};

    #[test]
    fn test_snap_mgr_create_dir() {
        // Ensure `mgr` creates the specified directory when it does not exist.
        let temp_dir = TempDir::new("test-snap-mgr-create-dir").unwrap();
        let temp_path = temp_dir.path().join("snap1");
        let path = temp_path.to_str().unwrap().to_owned();
        assert!(!temp_path.exists());
        let mut mgr = new_snap_mgr(path, None, false);
        mgr.wl().init().unwrap();
        assert!(temp_path.exists());

        // Ensure `init()` will return an error if specified target is a file.
        let temp_path2 = temp_dir.path().join("snap2");
        let path2 = temp_path2.to_str().unwrap().to_owned();
        File::create(temp_path2).unwrap();
        mgr = new_snap_mgr(path2, None, false);
        assert!(mgr.wl().init().is_err());
    }

    #[test]
    fn test_snap_mgr_v1() {
        // Ensure `mgr` is of size 0 when it's initialized.
        let temp_dir = TempDir::new("test-snap-mgr-v1").unwrap();
        let path = temp_dir.path().to_str().unwrap().to_owned();
        let mgr = new_snap_mgr(path.clone(), None, false);
        mgr.wl().init().unwrap();
        assert_eq!(mgr.rl().get_total_snap_size(), 0);

        let key1 = SnapKey::new(1, 1, 1);
        let size_track = Arc::new(RwLock::new(0));
        let mut s1 = SnapV1::new_for_writing(&path, size_track.clone(), true, &key1).unwrap();
        let test_data = b"test_data";
        let expected_size = (test_data.len() + CRC32_BYTES_COUNT) as u64;
        s1.write_all(test_data).unwrap();
        s1.save_with_checksum().unwrap();
        let mut s2 = SnapV1::new_for_writing(&path, size_track.clone(), false, &key1).unwrap();
        s2.write_all(test_data).unwrap();
        s2.save_with_checksum().unwrap();

        let key2 = SnapKey::new(2, 1, 1);
        let s3 = SnapV1::new_for_writing(&path, size_track.clone(), true, &key2).unwrap();
        let s4 = SnapV1::new_for_writing(&path, size_track.clone(), false, &key2).unwrap();

        assert!(s1.exists());
        assert!(s2.exists());
        assert!(!s3.exists());
        assert!(!s4.exists());

        // Ensure `mgr` would delete all the temporary files on initialization if they exist.
        let mgr = new_snap_mgr(path, None, false);
        mgr.wl().init().unwrap();
        assert_eq!(mgr.rl().get_total_snap_size(), expected_size * 2);

        assert!(s1.exists());
        assert!(s2.exists());
        assert!(!s3.exists());
        assert!(!s4.exists());

        // Ensure `mgr` tracks the size of all snapshots correctly when deleting snapshots.
        mgr.rl().get_snapshot_for_sending(&key1).unwrap().delete();
        assert_eq!(mgr.rl().get_total_snap_size(), expected_size);
        mgr.rl().get_snapshot_for_applying(&key1).unwrap().delete();
        assert_eq!(mgr.rl().get_total_snap_size(), 0);
    }

    #[test]
    fn test_snap_mgr_v2() {
        let temp_dir = TempDir::new("test-snap-mgr-v2").unwrap();
        let path = temp_dir.path().to_str().unwrap().to_owned();
        let mgr = new_snap_mgr(path.clone(), None, true);
        mgr.wl().init().unwrap();
        assert_eq!(mgr.rl().get_total_snap_size(), 0);

        let db_dir = TempDir::new("test-snap-mgr-delete-temp-files-v2-db").unwrap();
        let snapshot = DbSnapshot::new(v2::test::get_test_db(&db_dir).unwrap());
        let key1 = SnapKey::new(1, 1, 1);
        let size_track = Arc::new(RwLock::new(0));
        let mut s1 = SnapV2::new_for_building(&path, &key1, &snapshot, size_track.clone()).unwrap();
        let mut region = v2::test::get_test_region(1, 1, 1);
        let mut snap_data = RaftSnapshotData::new();
        snap_data.set_region(region.clone());
        let mut context = BuildContext::new();
        s1.build(&snapshot, &region, &mut snap_data, &mut context).unwrap();
        let mut s = SnapV2::new_for_sending(&path, &key1, size_track.clone()).unwrap();
        let expected_size = s.total_size().unwrap();
        let mut s2 = SnapV2::new_for_receiving(&path,
                                               &key1,
                                               snap_data.get_meta().clone(),
                                               size_track.clone())
            .unwrap();
        let n = io::copy(&mut s, &mut s2).unwrap();
        assert_eq!(n, expected_size);
        s2.save().unwrap();

        let key2 = SnapKey::new(2, 1, 1);
        region.set_id(2);
        snap_data.set_region(region);
        let s3 = SnapV2::new_for_building(&path, &key2, &snapshot, size_track.clone()).unwrap();
        let s4 = SnapV2::new_for_receiving(&path, &key2, snap_data.take_meta(), size_track.clone())
            .unwrap();

        assert!(s1.exists());
        assert!(s2.exists());
        assert!(!s3.exists());
        assert!(!s4.exists());

        let mgr = new_snap_mgr(path, None, true);
        mgr.wl().init().unwrap();
        assert_eq!(mgr.rl().get_total_snap_size(), expected_size * 2);

        assert!(s1.exists());
        assert!(s2.exists());
        assert!(!s3.exists());
        assert!(!s4.exists());

        mgr.rl().get_snapshot_for_sending(&key1).unwrap().delete();
        assert_eq!(mgr.rl().get_total_snap_size(), expected_size);
        mgr.rl().get_snapshot_for_applying(&key1).unwrap().delete();
        assert_eq!(mgr.rl().get_total_snap_size(), 0);
    }

    #[test]
    fn test_snap_v1_v2_compatible() {
        // Ensure that it's compatible between v1 impl and v2 impl, which means a receiver
        // running v2 code could receive and apply the snapshot sent from a sender running v1 code.
        let src_temp_dir = TempDir::new("test-snap-v1-v2-compatible-src").unwrap();
        let src_path = src_temp_dir.path().to_str().unwrap().to_owned();
        let src_mgr = new_snap_mgr(src_path, None, false);
        src_mgr.wl().init().unwrap();

        let src_db_dir = TempDir::new("test-snap-v1-v2-compatible-src-db").unwrap();
        let db = v2::test::get_test_db(&src_db_dir).unwrap();
        let snapshot = DbSnapshot::new(db.clone());
        let key = SnapKey::new(1, 1, 1);
        let mut s1 = src_mgr.rl().get_snapshot_for_building(&key, &snapshot).unwrap();
        let region = v2::test::get_test_region(1, 1, 1);
        let mut snap_data = RaftSnapshotData::new();
        snap_data.set_region(region.clone());
        let mut context = BuildContext::new();
        s1.build(&snapshot, &region, &mut snap_data, &mut context).unwrap();
        let mut v = vec![];
        snap_data.write_to_vec(&mut v).unwrap();

        let mut s2 = src_mgr.rl().get_snapshot_for_sending(&key).unwrap();
        let expected_size = s2.total_size().unwrap();

        let dst_temp_dir = TempDir::new("test-snap-v1-v2-compatible-dst").unwrap();
        let dst_path = dst_temp_dir.path().to_str().unwrap().to_owned();
        let dst_mgr = new_snap_mgr(dst_path, None, true);
        dst_mgr.wl().init().unwrap();

        let mut s3 = dst_mgr.rl().get_snapshot_for_receiving(&key, &v[..]).unwrap();
        let n = io::copy(&mut s2, &mut s3).unwrap();
        assert_eq!(n, expected_size);
        s3.save().unwrap();

        let dst_db_dir = TempDir::new("test-snap-v1-v2-compatible-dst-db").unwrap();
        let dst_db = Arc::new(rocksdb::new_engine(dst_db_dir.path().to_str().unwrap(), ALL_CFS)
            .unwrap());
        let options = ApplyOptions {
            db: dst_db.clone(),
            region: region.clone(),
            abort: Arc::new(AtomicUsize::new(JOB_STATUS_RUNNING)),
            write_batch_size: 10 * 1024 * 1024,
        };
        let mut s4 = dst_mgr.rl().get_snapshot_for_applying(&key).unwrap();
        s4.apply(options).unwrap();

        v2::test::assert_eq_db(db, dst_db.as_ref());
    }
}
