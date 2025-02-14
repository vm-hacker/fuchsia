// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/blobfs/directory.h"

#include <fuchsia/device/c/fidl.h>
#include <lib/fidl-utils/bind.h>
#include <lib/sync/completion.h>
#include <stdlib.h>
#include <string.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>

#include <string_view>
#include <utility>
#include <vector>

#include <fbl/ref_ptr.h>

#include "src/lib/digest/digest.h"
#include "src/storage/blobfs/blob.h"
#include "src/storage/blobfs/blobfs.h"

namespace blobfs {

Directory::Directory(Blobfs* bs) : Vnode(bs->vfs()), blobfs_(bs) {}

Directory::~Directory() = default;

zx_status_t Directory::GetNodeInfoForProtocol([[maybe_unused]] fs::VnodeProtocol protocol,
                                              [[maybe_unused]] fs::Rights rights,
                                              fs::VnodeRepresentation* info) {
  *info = fs::VnodeRepresentation::Directory();
  return ZX_OK;
}

fs::VnodeProtocolSet Directory::GetProtocols() const { return fs::VnodeProtocol::kDirectory; }

zx_status_t Directory::Readdir(fs::VdirCookie* cookie, void* dirents, size_t len,
                               size_t* out_actual) {
  return blobfs_->Readdir(cookie, dirents, len, out_actual);
}

zx_status_t Directory::Read(void* data, size_t len, size_t off, size_t* out_actual) {
  return ZX_ERR_NOT_FILE;
}

zx_status_t Directory::Write(const void* data, size_t len, size_t offset, size_t* out_actual) {
  return ZX_ERR_NOT_FILE;
}

zx_status_t Directory::Append(const void* data, size_t len, size_t* out_end, size_t* out_actual) {
  return ZX_ERR_NOT_FILE;
}

zx_status_t Directory::Lookup(std::string_view name, fbl::RefPtr<fs::Vnode>* out) {
  TRACE_DURATION("blobfs", "Directory::Lookup", "name", name);
  assert(memchr(name.data(), '/', name.length()) == nullptr);

  return blobfs_->node_operations().lookup.Track([&] {
    if (name == ".") {
      // Special case: Accessing root directory via '.'
      *out = fbl::RefPtr<Directory>(this);
      return ZX_OK;
    }

    zx_status_t status;
    Digest digest;
    if ((status = digest.Parse(name.data(), name.length())) != ZX_OK) {
      return status;
    }
    fbl::RefPtr<CacheNode> cache_node;
    if ((status = blobfs_->GetCache().Lookup(digest, &cache_node)) != ZX_OK) {
      return status;
    }
    auto vnode = fbl::RefPtr<Blob>::Downcast(std::move(cache_node));
    blobfs_->GetMetrics()->UpdateLookup(vnode->FileSize());
    *out = std::move(vnode);
    return ZX_OK;
  });
}

zx_status_t Directory::GetAttributes(fs::VnodeAttributes* a) {
  *a = fs::VnodeAttributes();
  a->mode = V_TYPE_DIR | V_IRUSR;
  a->inode = fuchsia_io::wire::kInoUnknown;
  a->content_size = 0;
  a->storage_size = 0;
  a->link_count = 1;
  a->creation_time = 0;
  a->modification_time = 0;
  return ZX_OK;
}

zx_status_t Directory::Create(std::string_view name, uint32_t mode, fbl::RefPtr<fs::Vnode>* out) {
  TRACE_DURATION("blobfs", "Directory::Create", "name", name, "mode", mode);
  assert(memchr(name.data(), '/', name.length()) == nullptr);

  CompressionAlgorithm data_format = CompressionAlgorithm::kUncompressed;
  // Handle case where this is a pre-compressed blob.
  {
    auto found_pos = name.rfind(kChunkedFileExtension);
    if (found_pos != std::string_view::npos) {
      // Ensure offline compression is enabled, otherwise disallow the request.
      if (!blobfs_->allow_offline_compression()) {
        return ZX_ERR_NOT_SUPPORTED;
      }
      data_format = CompressionAlgorithm::kChunked;
      // Remove extension from hash.
      name = name.substr(0, found_pos);
    }
  }

  return blobfs_->node_operations().create.Track([&] {
    Digest digest;
    zx_status_t status;
    if ((status = digest.Parse(name.data(), name.length())) != ZX_OK) {
      return status;
    }

    fbl::RefPtr<Blob> vn = fbl::AdoptRef(new Blob(blobfs_, std::move(digest), data_format));
    if ((status = blobfs_->GetCache().Add(vn)) != ZX_OK) {
      return status;
    }
    if ((status = vn->OpenValidating(fs::VnodeConnectionOptions(), nullptr)) != ZX_OK) {
      return status;
    }
    *out = std::move(vn);
    return ZX_OK;
  });
}

zx::status<std::string> Directory::GetDevicePath() const {
  return blobfs_->Device()->GetDevicePath();
}

zx_status_t Directory::Unlink(std::string_view name, bool must_be_dir) {
  TRACE_DURATION("blobfs", "Directory::Unlink", "name", name, "must_be_dir", must_be_dir);
  assert(memchr(name.data(), '/', name.length()) == nullptr);

  return blobfs_->node_operations().unlink.Track([&] {
    zx_status_t status;
    Digest digest;
    if ((status = digest.Parse(name.data(), name.length())) != ZX_OK) {
      return status;
    }
    fbl::RefPtr<CacheNode> cache_node;
    if ((status = blobfs_->GetCache().Lookup(digest, &cache_node)) != ZX_OK) {
      return status;
    }
    auto vnode = fbl::RefPtr<Blob>::Downcast(std::move(cache_node));
    blobfs_->GetMetrics()->UpdateLookup(vnode->FileSize());
    status = vnode->QueueUnlink();
    return status;
  });
}

void Directory::Sync(SyncCallback closure) {
  auto event = blobfs_->node_operations().sync.NewEvent();
  blobfs_->Sync(
      [this, cb = std::move(closure), event = std::move(event)](zx_status_t status) mutable {
        // This callback will be issued on the journal thread in the normal case. This is important
        // because the flush must happen there or it will block the main thread which would block
        // processing other requests.
        //
        // If called during shutdown this may get issued on the main thread but then the flush
        // transaction should be a no-op.
        if (status == ZX_OK) {
          status = blobfs_->Flush();
        }
        cb(status);
        event.SetStatus(status);
      });
}

void Directory::HandleFsSpecificMessage(fidl::IncomingHeaderAndMessage& msg,
                                        fidl::Transaction* txn) {
  fidl::WireDispatch<fuchsia_blobfs::Blobfs>(this, std::move(msg), txn);
}

void Directory::GetAllocatedRegions(GetAllocatedRegionsRequestView request,
                                    GetAllocatedRegionsCompleter::Sync& completer) {
  static_assert(sizeof(fuchsia_blobfs::wire::BlockRegion) == sizeof(BlockRegion));
  static_assert(offsetof(fuchsia_blobfs::wire::BlockRegion, offset) ==
                offsetof(BlockRegion, offset));
  static_assert(offsetof(fuchsia_blobfs::wire::BlockRegion, length) ==
                offsetof(BlockRegion, length));
  zx::vmo vmo;
  zx_status_t status = ZX_OK;
  std::vector<BlockRegion> buffer = blobfs_->GetAllocator()->GetAllocatedRegions();
  uint64_t allocations = buffer.size();
  if (allocations != 0) {
    status = zx::vmo::create(sizeof(BlockRegion) * allocations, 0, &vmo);
    if (status == ZX_OK) {
      status = vmo.write(buffer.data(), 0, sizeof(BlockRegion) * allocations);
    }
  }
  if (status == ZX_OK) {
    completer.Reply(ZX_OK, std::move(vmo), allocations);
  } else {
    completer.Reply(status, zx::vmo(), 0);
  }
}

void Directory::SetCorruptBlobHandler(SetCorruptBlobHandlerRequestView request,
                                      SetCorruptBlobHandlerCompleter::Sync& completer) {
  blobfs_->SetCorruptBlobHandler(std::move(request->handler));
  completer.Reply(ZX_OK);
}

}  // namespace blobfs
