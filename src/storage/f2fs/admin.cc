// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/f2fs/f2fs.h"

namespace f2fs {

#ifdef __Fuchsia__
AdminService::AdminService(async_dispatcher_t* dispatcher, ShutdownRequester shutdown)
    : fs::Service([dispatcher, this](fidl::ServerEnd<fuchsia_fs::Admin> server_end) {
        return fidl::BindSingleInFlightOnly(dispatcher, std::move(server_end), this);
      }),
      shutdown_(std::move(shutdown)) {}

void AdminService::Shutdown(ShutdownRequestView request, ShutdownCompleter::Sync& completer) {
  shutdown_([completer = completer.ToAsync()](zx_status_t status) mutable {
    if (status != ZX_OK) {
      FX_LOGS(ERROR) << "filesystem shutdown failed: " << zx_status_get_string(status);
    }
    completer.Reply();
  });
}

#endif  // __Fuchsia__

}  // namespace f2fs
