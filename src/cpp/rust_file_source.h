#pragma once

// Rust-backed FileSource bridge.
//
// Installs an `mbgl::FileSource` factory for `FileSourceType::ResourceLoader`
// that delegates every resource request to a Rust closure. This replaces the
// default ResourceLoader (which composes Asset/Database/Network/Mbtiles/Pmtiles
// sources) with a single Rust-supplied handler — letting callers serve
// mbtiles://, file://, and custom schemes from Rust without running a sidecar
// HTTP server or pre-extracting tiles.
//
// Factory registration is process-global (mbgl::FileSourceManager is a
// singleton). Call `register_rust_file_source_factory` once before any
// `mbgl::Map` is constructed. A subsequent call replaces the previous
// callback but leaves existing RustFileSource instances alive until their
// owning Map is destroyed.
//
// Two dispatch modes; the async path is enabled only when build.rs sets
// `MLN_ASYNC_FILE_SOURCE` (mirroring the cargo `async` feature):
//
//  - Sync: `RustFileSource::request` invokes the Rust closure inline and
//    delivers the response before returning. Returns a `NoopAsyncRequest`
//    because cancellation is structurally a no-op.
//
//  - Async: `RustFileSource::request` builds an `FsRequestSink` (holding
//    the mbgl `Callback` plus a cancellation flag), hands it to Rust which
//    spawns a tokio task, and returns a `RustAsyncRequest`. When mbgl drops
//    the `RustAsyncRequest`, the sink's `cancelled` flag is set; if the
//    Rust task delivers later, the sink swallows the response.

#include "rust/cxx.h"

#include <atomic>
#include <functional>
#include <memory>
#include <mutex>

#include <mbgl/storage/response.hpp>

namespace mln {
namespace bridge {

// Opaque Rust types — defined in src/renderer/file_source.rs.
struct FileSourceRequestCallback;
struct RustFsResponse;

// Shared state between an in-flight request's `RustAsyncRequest` (held by
// mbgl as the cancellation handle) and its `FsRequestSink` (held by the
// spawned Rust task). Owned by `shared_ptr` on both sides. Always defined
// so the cxx-generated bridge can take pointers to `FsRequestSink` even on
// sync-only builds; the async-specific call sites in `rust_file_source.cpp`
// stay gated behind `MLN_ASYNC_FILE_SOURCE`.
struct FsRequestShared {
    std::mutex mu;
    std::atomic<bool> cancelled{false};
    // mbgl's per-request callback. Cleared after delivery or on cancellation.
    std::function<void(mbgl::Response)> cb;
};

class FsRequestSink {
public:
    explicit FsRequestSink(std::shared_ptr<FsRequestShared> shared) noexcept
        : shared_(std::move(shared)) {}

    void deliver(RustFsResponse response) noexcept;

private:
    std::shared_ptr<FsRequestShared> shared_;
};

// Implementation in rust_file_source.cpp.
void register_rust_file_source_factory(rust::Box<FileSourceRequestCallback> callback);

} // namespace bridge
} // namespace mln
