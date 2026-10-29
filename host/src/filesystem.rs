//! `wasi:filesystem` for a component with a mounted `dir`: wasmtime's
//! implementation, with each refusal recorded.
//!
//! wasmtime enforces the mount itself: a path that leads outside the `dir`
//! (`..`, or a symlink pointing out) and a write under a read-only one fail
//! with `not-permitted`. Those used to reach the guest as a plain error that
//! no audit record mentioned. Every call here goes to wasmtime unchanged, and
//! a `not-permitted` result is recorded as `filesystem.denied`, saying why
//! (`outside_dir` or `read_only`) and what kind of operation it was (`read`,
//! `write` or `metadata`), never the path, which may be sensitive. The WIT
//! function isn't recorded either: it's whichever low-level call the guest's
//! libc made first (`metadata-hash-at` for a `stat`), and reads to a reviewer
//! as intent that isn't there. Other errors, such as a missing file or an OS
//! permission error (`access`), are ordinary failures and aren't recorded.
//!
//! wasmtime returns the same error for both refusals, so the reason comes from
//! the operation: a write under a read-only `dir` is `read_only` (wasmtime
//! checks that before the path), any other refused operation on a path is
//! `outside_dir`. An OS `EPERM` also comes back as `not-permitted`; it's rare,
//! and counting it as a denial errs on the side of a review. On a descriptor
//! rather than a path, where nothing can lead outside, only the OS refuses
//! anything else, recorded as `not_permitted`.
use wasmtime::Result;
use wasmtime::component::{HasData, Linker, Resource};
use wasmtime_wasi::filesystem::{Descriptor, WasiFilesystemCtxView, WasiFilesystemView};
use wasmtime_wasi::p2::bindings::filesystem::preopens;
use wasmtime_wasi::p2::bindings::filesystem::types::{
    self, ErrorCode, HostDescriptor, HostDirectoryEntryStream,
};
use wasmtime_wasi::p2::{FsError, FsResult};
use wasmtime_wasi_io::streams::{DynInputStream, DynOutputStream};

use crate::{SandboxAudit, SandboxState, audit};

/// Links `wasi:filesystem` 0.2, recording refusals.
pub(crate) fn add_to_linker(linker: &mut Linker<SandboxState>) -> Result<()> {
    types::add_to_linker::<_, Audited>(linker, audited)?;
    preopens::add_to_linker::<_, Audited>(linker, audited)?;
    Ok(())
}

struct Audited;

impl HasData for Audited {
    type Data<'a> = AuditedFilesystem<'a>;
}

fn audited(state: &mut SandboxState) -> AuditedFilesystem<'_> {
    let audit = state.hooks.audit.clone();
    let dir_writable = state.dir_writable;
    AuditedFilesystem {
        inner: state.filesystem(),
        audit,
        dir_writable,
    }
}

/// wasmtime's filesystem view, plus where its refusals are recorded.
pub(crate) struct AuditedFilesystem<'a> {
    inner: WasiFilesystemCtxView<'a>,
    audit: SandboxAudit,
    dir_writable: bool,
}

/// What kind of operation was refused.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Read,
    Write,
    Metadata,
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Op::Read => "read",
            Op::Write => "write",
            Op::Metadata => "metadata",
        }
    }
}

/// Why wasmtime refused `op`, on a path (`at_path`) or a descriptor, under a
/// `dir` that's writable or not.
fn reason(op: Op, at_path: bool, dir_writable: bool) -> &'static str {
    match (op, at_path) {
        (Op::Write, _) if !dir_writable => "read_only",
        (_, true) => "outside_dir",
        (_, false) => "not_permitted",
    }
}

impl AuditedFilesystem<'_> {
    /// Records `result` if wasmtime refused `op` on a descriptor, and passes
    /// it on unchanged.
    fn check<T>(&self, op: Op, result: FsResult<T>) -> FsResult<T> {
        self.record(op, false, &result);
        result
    }

    /// Like `check`, for an operation on a path under a descriptor.
    fn check_at<T>(&self, op: Op, result: FsResult<T>) -> FsResult<T> {
        self.record(op, true, &result);
        result
    }

    fn record<T>(&self, op: Op, at_path: bool, result: &FsResult<T>) {
        if let Err(error) = result
            && matches!(error.downcast_ref(), Some(ErrorCode::NotPermitted))
        {
            let fields = serde_json::Map::from_iter([("operation".to_string(), op.name().into())]);
            self.audit.cell.note(
                &self.audit.id,
                "filesystem.denied",
                audit::Decision::Denied,
                reason(op, at_path, self.dir_writable),
                fields,
            );
        }
    }
}

// The rest delegates to wasmtime, one method per WIT function.

impl preopens::Host for AuditedFilesystem<'_> {
    fn get_directories(&mut self) -> wasmtime::Result<Vec<(Resource<Descriptor>, String)>> {
        preopens::Host::get_directories(&mut self.inner)
    }
}

impl types::Host for AuditedFilesystem<'_> {
    fn convert_error_code(&mut self, err: FsError) -> wasmtime::Result<ErrorCode> {
        types::Host::convert_error_code(&mut self.inner, err)
    }

    fn filesystem_error_code(
        &mut self,
        err: Resource<wasmtime::Error>,
    ) -> wasmtime::Result<Option<ErrorCode>> {
        types::Host::filesystem_error_code(&mut self.inner, err)
    }
}

impl HostDescriptor for AuditedFilesystem<'_> {
    async fn advise(
        &mut self,
        fd: Resource<types::Descriptor>,
        offset: types::Filesize,
        len: types::Filesize,
        advice: types::Advice,
    ) -> FsResult<()> {
        let result = HostDescriptor::advise(&mut self.inner, fd, offset, len, advice).await;
        self.check(Op::Read, result)
    }

    async fn sync_data(&mut self, fd: Resource<types::Descriptor>) -> FsResult<()> {
        let result = HostDescriptor::sync_data(&mut self.inner, fd).await;
        self.check(Op::Write, result)
    }

    async fn get_flags(
        &mut self,
        fd: Resource<types::Descriptor>,
    ) -> FsResult<types::DescriptorFlags> {
        let result = HostDescriptor::get_flags(&mut self.inner, fd).await;
        self.check(Op::Metadata, result)
    }

    async fn get_type(
        &mut self,
        fd: Resource<types::Descriptor>,
    ) -> FsResult<types::DescriptorType> {
        let result = HostDescriptor::get_type(&mut self.inner, fd).await;
        self.check(Op::Metadata, result)
    }

    async fn set_size(
        &mut self,
        fd: Resource<types::Descriptor>,
        size: types::Filesize,
    ) -> FsResult<()> {
        let result = HostDescriptor::set_size(&mut self.inner, fd, size).await;
        self.check(Op::Write, result)
    }

    async fn set_times(
        &mut self,
        fd: Resource<types::Descriptor>,
        atim: types::NewTimestamp,
        mtim: types::NewTimestamp,
    ) -> FsResult<()> {
        let result = HostDescriptor::set_times(&mut self.inner, fd, atim, mtim).await;
        self.check(Op::Write, result)
    }

    async fn read(
        &mut self,
        fd: Resource<types::Descriptor>,
        len: types::Filesize,
        offset: types::Filesize,
    ) -> FsResult<(Vec<u8>, bool)> {
        let result = HostDescriptor::read(&mut self.inner, fd, len, offset).await;
        self.check(Op::Read, result)
    }

    async fn write(
        &mut self,
        fd: Resource<types::Descriptor>,
        buf: Vec<u8>,
        offset: types::Filesize,
    ) -> FsResult<types::Filesize> {
        let result = HostDescriptor::write(&mut self.inner, fd, buf, offset).await;
        self.check(Op::Write, result)
    }

    async fn read_directory(
        &mut self,
        fd: Resource<types::Descriptor>,
    ) -> FsResult<Resource<types::DirectoryEntryStream>> {
        let result = HostDescriptor::read_directory(&mut self.inner, fd).await;
        self.check(Op::Read, result)
    }

    async fn sync(&mut self, fd: Resource<types::Descriptor>) -> FsResult<()> {
        let result = HostDescriptor::sync(&mut self.inner, fd).await;
        self.check(Op::Write, result)
    }

    async fn create_directory_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path: String,
    ) -> FsResult<()> {
        let result = HostDescriptor::create_directory_at(&mut self.inner, fd, path).await;
        self.check_at(Op::Write, result)
    }

    async fn stat(&mut self, fd: Resource<types::Descriptor>) -> FsResult<types::DescriptorStat> {
        let result = HostDescriptor::stat(&mut self.inner, fd).await;
        self.check(Op::Metadata, result)
    }

    async fn stat_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path_flags: types::PathFlags,
        path: String,
    ) -> FsResult<types::DescriptorStat> {
        let result = HostDescriptor::stat_at(&mut self.inner, fd, path_flags, path).await;
        self.check_at(Op::Metadata, result)
    }

    async fn set_times_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path_flags: types::PathFlags,
        path: String,
        atim: types::NewTimestamp,
        mtim: types::NewTimestamp,
    ) -> FsResult<()> {
        let result =
            HostDescriptor::set_times_at(&mut self.inner, fd, path_flags, path, atim, mtim).await;
        self.check_at(Op::Write, result)
    }

    async fn link_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        old_path_flags: types::PathFlags,
        old_path: String,
        new_descriptor: Resource<types::Descriptor>,
        new_path: String,
    ) -> FsResult<()> {
        let result = HostDescriptor::link_at(
            &mut self.inner,
            fd,
            old_path_flags,
            old_path,
            new_descriptor,
            new_path,
        )
        .await;
        self.check_at(Op::Write, result)
    }

    async fn open_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path_flags: types::PathFlags,
        path: String,
        oflags: types::OpenFlags,
        flags: types::DescriptorFlags,
    ) -> FsResult<Resource<types::Descriptor>> {
        // What wasmtime counts as a write when it checks a read-only mount.
        let writes = flags.contains(types::DescriptorFlags::WRITE)
            || oflags.intersects(types::OpenFlags::CREATE | types::OpenFlags::TRUNCATE);
        let op = if writes { Op::Write } else { Op::Read };
        let result =
            HostDescriptor::open_at(&mut self.inner, fd, path_flags, path, oflags, flags).await;
        self.check_at(op, result)
    }

    fn drop(&mut self, fd: Resource<types::Descriptor>) -> wasmtime::Result<()> {
        HostDescriptor::drop(&mut self.inner, fd)
    }

    async fn readlink_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path: String,
    ) -> FsResult<String> {
        let result = HostDescriptor::readlink_at(&mut self.inner, fd, path).await;
        self.check_at(Op::Metadata, result)
    }

    async fn remove_directory_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path: String,
    ) -> FsResult<()> {
        let result = HostDescriptor::remove_directory_at(&mut self.inner, fd, path).await;
        self.check_at(Op::Write, result)
    }

    async fn rename_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        old_path: String,
        new_fd: Resource<types::Descriptor>,
        new_path: String,
    ) -> FsResult<()> {
        let result =
            HostDescriptor::rename_at(&mut self.inner, fd, old_path, new_fd, new_path).await;
        self.check_at(Op::Write, result)
    }

    async fn symlink_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        src_path: String,
        dest_path: String,
    ) -> FsResult<()> {
        let result = HostDescriptor::symlink_at(&mut self.inner, fd, src_path, dest_path).await;
        self.check_at(Op::Write, result)
    }

    async fn unlink_file_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path: String,
    ) -> FsResult<()> {
        let result = HostDescriptor::unlink_file_at(&mut self.inner, fd, path).await;
        self.check_at(Op::Write, result)
    }

    fn read_via_stream(
        &mut self,
        fd: Resource<types::Descriptor>,
        offset: types::Filesize,
    ) -> FsResult<Resource<DynInputStream>> {
        let result = HostDescriptor::read_via_stream(&mut self.inner, fd, offset);
        self.check(Op::Read, result)
    }

    fn write_via_stream(
        &mut self,
        fd: Resource<types::Descriptor>,
        offset: types::Filesize,
    ) -> FsResult<Resource<DynOutputStream>> {
        let result = HostDescriptor::write_via_stream(&mut self.inner, fd, offset);
        self.check(Op::Write, result)
    }

    fn append_via_stream(
        &mut self,
        fd: Resource<types::Descriptor>,
    ) -> FsResult<Resource<DynOutputStream>> {
        let result = HostDescriptor::append_via_stream(&mut self.inner, fd);
        self.check(Op::Write, result)
    }

    async fn is_same_object(
        &mut self,
        a: Resource<types::Descriptor>,
        b: Resource<types::Descriptor>,
    ) -> wasmtime::Result<bool> {
        HostDescriptor::is_same_object(&mut self.inner, a, b).await
    }

    async fn metadata_hash(
        &mut self,
        fd: Resource<types::Descriptor>,
    ) -> FsResult<types::MetadataHashValue> {
        let result = HostDescriptor::metadata_hash(&mut self.inner, fd).await;
        self.check(Op::Metadata, result)
    }

    async fn metadata_hash_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path_flags: types::PathFlags,
        path: String,
    ) -> FsResult<types::MetadataHashValue> {
        let result = HostDescriptor::metadata_hash_at(&mut self.inner, fd, path_flags, path).await;
        self.check_at(Op::Metadata, result)
    }
}

impl HostDirectoryEntryStream for AuditedFilesystem<'_> {
    async fn read_directory_entry(
        &mut self,
        stream: Resource<types::DirectoryEntryStream>,
    ) -> FsResult<Option<types::DirectoryEntry>> {
        let result = HostDirectoryEntryStream::read_directory_entry(&mut self.inner, stream).await;
        self.check(Op::Read, result)
    }

    fn drop(&mut self, stream: Resource<types::DirectoryEntryStream>) -> wasmtime::Result<()> {
        HostDirectoryEntryStream::drop(&mut self.inner, stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_says_which_way_it_broke_the_mount() {
        for (op, at_path, writable, expected) in [
            (Op::Read, true, false, "outside_dir"),
            (Op::Metadata, true, true, "outside_dir"),
            (Op::Write, true, true, "outside_dir"),
            (Op::Write, true, false, "read_only"),
            (Op::Write, false, false, "read_only"),
            (Op::Write, false, true, "not_permitted"),
            (Op::Read, false, false, "not_permitted"),
        ] {
            assert_eq!(
                reason(op, at_path, writable),
                expected,
                "{op:?} at_path={at_path} writable={writable}"
            );
        }
    }
}
