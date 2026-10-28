//! `wasi:filesystem` for a component with a mounted `dir`: wasmtime's
//! implementation, with each refusal recorded.
//!
//! wasmtime enforces the mount itself: a path that leads outside the `dir`
//! (`..`, or a symlink pointing out) and a write under a read-only one fail
//! with `not-permitted`. Those used to reach the guest as a plain error that
//! no audit record mentioned. Every call here goes to wasmtime unchanged, and
//! a `not-permitted` result is recorded as `filesystem.denied`, with the
//! operation but never its path, which may be sensitive. Other errors, such as
//! a missing file or an OS permission error (`access`), are ordinary failures
//! and aren't recorded. An OS `EPERM` also comes back as `not-permitted`; it's
//! rare, and counting it as a denial errs on the side of a review.
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
    AuditedFilesystem {
        inner: state.filesystem(),
        audit,
    }
}

/// wasmtime's filesystem view, plus where its refusals are recorded.
pub(crate) struct AuditedFilesystem<'a> {
    inner: WasiFilesystemCtxView<'a>,
    audit: SandboxAudit,
}

impl AuditedFilesystem<'_> {
    /// Records `result` if wasmtime refused `operation` (a WIT function
    /// name, e.g. `open-at`), and passes it on unchanged.
    fn check<T>(&self, operation: &'static str, result: FsResult<T>) -> FsResult<T> {
        if let Err(error) = &result
            && matches!(error.downcast_ref(), Some(ErrorCode::NotPermitted))
        {
            let fields = serde_json::Map::from_iter([("operation".to_string(), operation.into())]);
            self.audit.cell.note(
                &self.audit.id,
                "filesystem.denied",
                audit::Decision::Denied,
                "not_permitted",
                fields,
            );
        }
        result
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
        self.check("advise", result)
    }

    async fn sync_data(&mut self, fd: Resource<types::Descriptor>) -> FsResult<()> {
        let result = HostDescriptor::sync_data(&mut self.inner, fd).await;
        self.check("sync-data", result)
    }

    async fn get_flags(
        &mut self,
        fd: Resource<types::Descriptor>,
    ) -> FsResult<types::DescriptorFlags> {
        let result = HostDescriptor::get_flags(&mut self.inner, fd).await;
        self.check("get-flags", result)
    }

    async fn get_type(
        &mut self,
        fd: Resource<types::Descriptor>,
    ) -> FsResult<types::DescriptorType> {
        let result = HostDescriptor::get_type(&mut self.inner, fd).await;
        self.check("get-type", result)
    }

    async fn set_size(
        &mut self,
        fd: Resource<types::Descriptor>,
        size: types::Filesize,
    ) -> FsResult<()> {
        let result = HostDescriptor::set_size(&mut self.inner, fd, size).await;
        self.check("set-size", result)
    }

    async fn set_times(
        &mut self,
        fd: Resource<types::Descriptor>,
        atim: types::NewTimestamp,
        mtim: types::NewTimestamp,
    ) -> FsResult<()> {
        let result = HostDescriptor::set_times(&mut self.inner, fd, atim, mtim).await;
        self.check("set-times", result)
    }

    async fn read(
        &mut self,
        fd: Resource<types::Descriptor>,
        len: types::Filesize,
        offset: types::Filesize,
    ) -> FsResult<(Vec<u8>, bool)> {
        let result = HostDescriptor::read(&mut self.inner, fd, len, offset).await;
        self.check("read", result)
    }

    async fn write(
        &mut self,
        fd: Resource<types::Descriptor>,
        buf: Vec<u8>,
        offset: types::Filesize,
    ) -> FsResult<types::Filesize> {
        let result = HostDescriptor::write(&mut self.inner, fd, buf, offset).await;
        self.check("write", result)
    }

    async fn read_directory(
        &mut self,
        fd: Resource<types::Descriptor>,
    ) -> FsResult<Resource<types::DirectoryEntryStream>> {
        let result = HostDescriptor::read_directory(&mut self.inner, fd).await;
        self.check("read-directory", result)
    }

    async fn sync(&mut self, fd: Resource<types::Descriptor>) -> FsResult<()> {
        let result = HostDescriptor::sync(&mut self.inner, fd).await;
        self.check("sync", result)
    }

    async fn create_directory_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path: String,
    ) -> FsResult<()> {
        let result = HostDescriptor::create_directory_at(&mut self.inner, fd, path).await;
        self.check("create-directory-at", result)
    }

    async fn stat(&mut self, fd: Resource<types::Descriptor>) -> FsResult<types::DescriptorStat> {
        let result = HostDescriptor::stat(&mut self.inner, fd).await;
        self.check("stat", result)
    }

    async fn stat_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path_flags: types::PathFlags,
        path: String,
    ) -> FsResult<types::DescriptorStat> {
        let result = HostDescriptor::stat_at(&mut self.inner, fd, path_flags, path).await;
        self.check("stat-at", result)
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
        self.check("set-times-at", result)
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
        self.check("link-at", result)
    }

    async fn open_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path_flags: types::PathFlags,
        path: String,
        oflags: types::OpenFlags,
        flags: types::DescriptorFlags,
    ) -> FsResult<Resource<types::Descriptor>> {
        let result =
            HostDescriptor::open_at(&mut self.inner, fd, path_flags, path, oflags, flags).await;
        self.check("open-at", result)
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
        self.check("readlink-at", result)
    }

    async fn remove_directory_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path: String,
    ) -> FsResult<()> {
        let result = HostDescriptor::remove_directory_at(&mut self.inner, fd, path).await;
        self.check("remove-directory-at", result)
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
        self.check("rename-at", result)
    }

    async fn symlink_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        src_path: String,
        dest_path: String,
    ) -> FsResult<()> {
        let result = HostDescriptor::symlink_at(&mut self.inner, fd, src_path, dest_path).await;
        self.check("symlink-at", result)
    }

    async fn unlink_file_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path: String,
    ) -> FsResult<()> {
        let result = HostDescriptor::unlink_file_at(&mut self.inner, fd, path).await;
        self.check("unlink-file-at", result)
    }

    fn read_via_stream(
        &mut self,
        fd: Resource<types::Descriptor>,
        offset: types::Filesize,
    ) -> FsResult<Resource<DynInputStream>> {
        let result = HostDescriptor::read_via_stream(&mut self.inner, fd, offset);
        self.check("read-via-stream", result)
    }

    fn write_via_stream(
        &mut self,
        fd: Resource<types::Descriptor>,
        offset: types::Filesize,
    ) -> FsResult<Resource<DynOutputStream>> {
        let result = HostDescriptor::write_via_stream(&mut self.inner, fd, offset);
        self.check("write-via-stream", result)
    }

    fn append_via_stream(
        &mut self,
        fd: Resource<types::Descriptor>,
    ) -> FsResult<Resource<DynOutputStream>> {
        let result = HostDescriptor::append_via_stream(&mut self.inner, fd);
        self.check("append-via-stream", result)
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
        self.check("metadata-hash", result)
    }

    async fn metadata_hash_at(
        &mut self,
        fd: Resource<types::Descriptor>,
        path_flags: types::PathFlags,
        path: String,
    ) -> FsResult<types::MetadataHashValue> {
        let result = HostDescriptor::metadata_hash_at(&mut self.inner, fd, path_flags, path).await;
        self.check("metadata-hash-at", result)
    }
}

impl HostDirectoryEntryStream for AuditedFilesystem<'_> {
    async fn read_directory_entry(
        &mut self,
        stream: Resource<types::DirectoryEntryStream>,
    ) -> FsResult<Option<types::DirectoryEntry>> {
        let result = HostDirectoryEntryStream::read_directory_entry(&mut self.inner, stream).await;
        self.check("read-directory-entry", result)
    }

    fn drop(&mut self, stream: Resource<types::DirectoryEntryStream>) -> wasmtime::Result<()> {
        HostDirectoryEntryStream::drop(&mut self.inner, stream)
    }
}
