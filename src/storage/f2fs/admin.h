// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_F2FS_ADMIN_H_
#define SRC_STORAGE_F2FS_ADMIN_H_

namespace f2fs {

#ifdef __Fuchsia__
class AdminService final : public fidl::WireServer<fuchsia_fs::Admin>, public fs::Service {
 public:
  using ShutdownRequester = fit::callback<void(fs::FuchsiaVfs::ShutdownCallback)>;
  AdminService(async_dispatcher_t* dispatcher, ShutdownRequester shutdown);

  void Shutdown(ShutdownRequestView request, ShutdownCompleter::Sync& completer) final;

 private:
  ShutdownRequester shutdown_;
};
#endif  // __Fuchsia__

}  // namespace f2fs

#endif  // SRC_STORAGE_F2FS_ADMIN_H_
