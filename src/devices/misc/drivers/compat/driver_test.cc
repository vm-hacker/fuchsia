// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/misc/drivers/compat/driver.h"

#include <dirent.h>
#include <fidl/fuchsia.boot/cpp/wire_test_base.h>
#include <fidl/fuchsia.device.fs/cpp/wire_test_base.h>
#include <fidl/fuchsia.driver.framework/cpp/wire_test_base.h>
#include <fidl/fuchsia.io/cpp/wire_test_base.h>
#include <fidl/fuchsia.logger/cpp/wire.h>
#include <fidl/fuchsia.scheduler/cpp/wire_test_base.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fdf/testing.h>
#include <lib/fdio/directory.h>
#include <lib/fit/defer.h>
#include <lib/sync/cpp/completion.h>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/devices/lib/compat/symbols.h"
#include "src/devices/misc/drivers/compat/v1_test.h"
#include "src/lib/storage/vfs/cpp/managed_vfs.h"
#include "src/lib/storage/vfs/cpp/pseudo_dir.h"
#include "src/lib/storage/vfs/cpp/service.h"
#include "src/lib/testing/loop_fixture/test_loop_fixture.h"

namespace fboot = fuchsia_boot;
namespace fdata = fuchsia_data;
namespace fdf {
using namespace fuchsia_driver_framework;
}
namespace fio = fuchsia_io;
namespace flogger = fuchsia_logger;
namespace frunner = fuchsia_component_runner;

constexpr auto kOpenFlags = fio::wire::OpenFlags::kRightReadable |
                            fio::wire::OpenFlags::kRightExecutable |
                            fio::wire::OpenFlags::kNotDirectory;
constexpr auto kVmoFlags = fio::wire::VmoFlags::kRead | fio::wire::VmoFlags::kExecute;

namespace {

zx::vmo GetVmo(std::string_view path) {
  zx::status endpoints = fidl::CreateEndpoints<fio::File>();
  EXPECT_TRUE(endpoints.is_ok()) << endpoints.status_string();
  zx_status_t status = fdio_open(path.data(), static_cast<uint32_t>(kOpenFlags),
                                 endpoints->server.channel().release());
  EXPECT_EQ(status, ZX_OK) << zx_status_get_string(status);
  fidl::WireResult result = fidl::WireCall(endpoints->client)->GetBackingMemory(kVmoFlags);
  EXPECT_TRUE(result.ok()) << result.FormatDescription();
  const auto& response = result.value();
  EXPECT_TRUE(response.is_ok()) << zx_status_get_string(response.error_value());
  return std::move(response.value()->vmo);
}

class TestNode : public fidl::testing::WireTestBase<fdf::Node> {
 public:
  bool HasChildren() const { return !controllers_.empty() || !nodes_.empty(); }

 private:
  void AddChild(AddChildRequestView request, AddChildCompleter::Sync& completer) override {
    controllers_.push_back(std::move(request->controller));
    nodes_.push_back(std::move(request->node));
    completer.ReplySuccess();
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    printf("Not implemented: Node::%s\n", name.data());
  }

  std::vector<fidl::ServerEnd<fdf::NodeController>> controllers_;
  std::vector<fidl::ServerEnd<fdf::Node>> nodes_;
};

class TestRootResource : public fidl::testing::WireTestBase<fboot::RootResource> {
 public:
  TestRootResource() { EXPECT_EQ(ZX_OK, zx::event::create(0, &fake_resource_)); }

 private:
  void Get(GetRequestView request, GetCompleter::Sync& completer) override {
    zx::event duplicate;
    ASSERT_EQ(ZX_OK, fake_resource_.duplicate(ZX_RIGHT_SAME_RIGHTS, &duplicate));
    completer.Reply(zx::resource(duplicate.release()));
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    printf("Not implemented: RootResource::%s\n", name.data());
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  // An event is similar enough that we can pretend it's the root resource, in that we can
  // send it over a FIDL channel.
  zx::event fake_resource_;
};

class TestItems : public fidl::testing::WireTestBase<fboot::Items> {
 private:
  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    printf("Not implemented: Items::%s\n", name.data());
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }
};

class TestFile : public fidl::testing::WireTestBase<fio::File> {
 public:
  void SetStatus(zx_status_t status) { status_ = status; }
  void SetVmo(zx::vmo vmo) { vmo_ = std::move(vmo); }

 private:
  void GetBackingMemory(GetBackingMemoryRequestView request,
                        GetBackingMemoryCompleter::Sync& completer) override {
    if (status_ != ZX_OK) {
      completer.ReplyError(status_);
    } else {
      completer.ReplySuccess(std::move(vmo_));
    }
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    printf("Not implemented: File::%s\n", name.data());
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  zx_status_t status_ = ZX_OK;
  zx::vmo vmo_;
};

class TestDirectory : public fidl::testing::WireTestBase<fio::Directory> {
 public:
  using OpenHandler = fit::function<void(OpenRequestView)>;

  void SetOpenHandler(OpenHandler open_handler) { open_handler_ = std::move(open_handler); }

 private:
  void Open(OpenRequestView request, OpenCompleter::Sync& completer) override {
    open_handler_(std::move(request));
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    printf("Not implemented: Directory::%s\n", name.data());
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  OpenHandler open_handler_;
};

class TestDevice : public fidl::WireServer<fuchsia_driver_compat::Device> {
  void GetTopologicalPath(GetTopologicalPathRequestView request,
                          GetTopologicalPathCompleter::Sync& completer) override {
    completer.Reply("/dev/test/my-device");
  }

  void GetMetadata(GetMetadataRequestView request, GetMetadataCompleter::Sync& completer) override {
    std::vector<fuchsia_driver_compat::wire::Metadata> metadata;

    std::vector<uint8_t> bytes_1 = {1, 2, 3};
    zx::vmo vmo_1;
    ASSERT_EQ(ZX_OK, zx::vmo::create(bytes_1.size(), 0, &vmo_1));
    vmo_1.write(bytes_1.data(), 0, bytes_1.size());
    size_t size = bytes_1.size();
    ASSERT_EQ(ZX_OK, vmo_1.set_property(ZX_PROP_VMO_CONTENT_SIZE, &size, sizeof(size)));
    metadata.push_back(fuchsia_driver_compat::wire::Metadata{.type = 1, .data = std::move(vmo_1)});

    std::vector<uint8_t> bytes_2 = {4, 5, 6};
    zx::vmo vmo_2;
    ASSERT_EQ(ZX_OK, zx::vmo::create(bytes_1.size(), 0, &vmo_2));
    vmo_2.write(bytes_2.data(), 0, bytes_2.size());
    ASSERT_EQ(ZX_OK, vmo_2.set_property(ZX_PROP_VMO_CONTENT_SIZE, &size, sizeof(size)));
    metadata.push_back(fuchsia_driver_compat::wire::Metadata{.type = 2, .data = std::move(vmo_2)});

    completer.ReplySuccess(fidl::VectorView<fuchsia_driver_compat::wire::Metadata>::FromExternal(
        metadata.data(), metadata.size()));
  }

  void ConnectFidl(ConnectFidlRequestView request, ConnectFidlCompleter::Sync& completer) override {
  }
};

class TestProfileProvider : public fidl::testing::WireTestBase<fuchsia_scheduler::ProfileProvider> {
 public:
  void GetProfile(GetProfileRequestView request, GetProfileCompleter::Sync& completer) override {
    if (get_profile_callback_) {
      get_profile_callback_(request->priority,
                            std::string_view(request->name.data(), request->name.size()));
    }
    completer.Reply(ZX_OK, zx::profile());
  }
  void SetGetProfileCallback(std::function<void(uint32_t, std::string_view)> cb) {
    get_profile_callback_ = std::move(cb);
  }

  void GetDeadlineProfile(GetDeadlineProfileRequestView request,
                          GetDeadlineProfileCompleter::Sync& completer) override {
    if (get_deadline_profile_callback_) {
      get_deadline_profile_callback_(request);
    }
    completer.Reply(ZX_OK, zx::profile());
  }
  void SetGetDeadlineProfileCallback(std::function<void(GetDeadlineProfileRequestView&)> cb) {
    get_deadline_profile_callback_ = std::move(cb);
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    printf("Not implemented: ProfileProvider::%s", name.data());
  }

 private:
  std::function<void(uint32_t, std::string_view)> get_profile_callback_;
  std::function<void(GetDeadlineProfileRequestView&)> get_deadline_profile_callback_;
};

class TestExporter : public fidl::testing::WireTestBase<fuchsia_device_fs::Exporter> {
 public:
  void Export(ExportRequestView request, ExportCompleter::Sync& completer) override {
    completer.ReplySuccess();
  }

  void ExportOptions(ExportOptionsRequestView request,
                     ExportOptionsCompleter::Sync& completer) override {
    completer.ReplySuccess();
  }

  void MakeVisible(MakeVisibleRequestView request, MakeVisibleCompleter::Sync& completer) override {
    completer.ReplySuccess();
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    printf("Not implemented: TestExporter::%s", name.data());
  }
};

}  // namespace

class DriverTest : public gtest::TestLoopFixture {
 protected:
  TestNode& node() { return node_; }
  TestFile& compat_file() { return compat_file_; }

  void SetUp() override {
    TestLoopFixture::SetUp();

    // When creating a new dispatcher, we need to associate it with some owner so that the driver
    // runtime library doesn't complain.
    fdf_testing_push_driver(this);
    auto pop_driver = fit::defer([]() { fdf_testing_pop_driver(); });

    auto dispatcher = fdf::Dispatcher::Create(
        0, "compat-test", [this](fdf_dispatcher_t* dispatcher) { dispatcher_shutdown_.Signal(); });
    EXPECT_EQ(ZX_OK, dispatcher.status_value());
    dispatcher_ = *std::move(dispatcher);

    fidl_loop_.StartThread("fidl-server-thread");
  }

  void TearDown() override {
    TestLoopFixture::TearDown();

    vfs_->Shutdown([](auto status) {});
    RunUntilDispatchersIdle();
  }

  std::unique_ptr<compat::Driver> StartDriver(std::string_view v1_driver_path,
                                              const zx_protocol_device_t* ops) {
    auto node_endpoints = fidl::CreateEndpoints<fdf::Node>();
    EXPECT_TRUE(node_endpoints.is_ok());
    auto outgoing_dir_endpoints = fidl::CreateEndpoints<fio::Directory>();
    EXPECT_TRUE(outgoing_dir_endpoints.is_ok());
    auto pkg_endpoints = fidl::CreateEndpoints<fio::Directory>();
    EXPECT_TRUE(pkg_endpoints.is_ok());
    auto svc_endpoints = fidl::CreateEndpoints<fio::Directory>();
    EXPECT_TRUE(svc_endpoints.is_ok());
    auto compat_service_endpoints = fidl::CreateEndpoints<fio::Directory>();
    EXPECT_TRUE(compat_service_endpoints.is_ok());

    // Setup the node.
    fidl::WireSharedClient<fuchsia_driver_framework::Node> node(std::move(node_endpoints->client),
                                                                dispatcher());
    fidl::BindServer(dispatcher(), std::move(node_endpoints->server), &node_);

    // Setup and bind "/pkg" directory.
    compat_file_.SetVmo(GetVmo("/pkg/driver/compat.so"));
    v1_test_file_.SetVmo(GetVmo(v1_driver_path));
    firmware_file_.SetVmo(GetVmo("/pkg/lib/firmware/test"));
    pkg_directory_.SetOpenHandler([this](TestDirectory::OpenRequestView request) {
      fidl::ServerEnd<fio::File> server_end(request->object.TakeChannel());
      if (request->path.get() == "driver/compat.so") {
        fidl::BindServer(dispatcher(), std::move(server_end), &compat_file_);
      } else if (request->path.get() == "driver/v1_test.so") {
        fidl::BindServer(dispatcher(), std::move(server_end), &v1_test_file_);
      } else if (request->path.get() == "lib/firmware/test") {
        fidl::BindServer(dispatcher(), std::move(server_end), &firmware_file_);
      } else {
        FAIL() << "Unexpected file: " << request->path.get();
      }
    });
    fidl::BindServer(dispatcher(), std::move(pkg_endpoints->server), &pkg_directory_);

    // Setup and bind "/svc" directory.
    {
      auto svc = fbl::MakeRefCounted<fs::PseudoDir>();
      svc->AddEntry(fidl::DiscoverableProtocolName<flogger::LogSink>,
                    fbl::MakeRefCounted<fs::Service>([](zx::channel server) {
                      return fdio_service_connect_by_name(
                          fidl::DiscoverableProtocolName<flogger::LogSink>, server.release());
                    }));

      svc->AddEntry(fidl::DiscoverableProtocolName<fboot::RootResource>,
                    fbl::MakeRefCounted<fs::Service>([this](zx::channel server) {
                      fidl::ServerEnd<fboot::RootResource> server_end(std::move(server));
                      fidl::BindServer(dispatcher(), std::move(server_end), &root_resource_);
                      return ZX_OK;
                    }));

      svc->AddEntry(fidl::DiscoverableProtocolName<fboot::Items>,
                    fbl::MakeRefCounted<fs::Service>([this](zx::channel server) {
                      fidl::ServerEnd<fboot::Items> server_end(std::move(server));
                      fidl::BindServer(dispatcher(), std::move(server_end), &items_);
                      return ZX_OK;
                    }));

      svc->AddEntry(fidl::DiscoverableProtocolName<fuchsia_device_fs::Exporter>,
                    fbl::MakeRefCounted<fs::Service>([this](zx::channel server) {
                      fidl::ServerEnd<fuchsia_device_fs::Exporter> server_end(std::move(server));
                      fidl::BindServer(fidl_loop_.dispatcher(), std::move(server_end), &exporter_);
                      return ZX_OK;
                    }));

      svc->AddEntry(
          fidl::DiscoverableProtocolName<fuchsia_scheduler::ProfileProvider>,
          fbl::MakeRefCounted<fs::Service>([this](zx::channel server) {
            fidl::ServerEnd<fuchsia_scheduler::ProfileProvider> server_end(std::move(server));
            fidl::BindServer(dispatcher(), std::move(server_end), &profile_provider_);
            return ZX_OK;
          }));

      auto compat_dir = fbl::MakeRefCounted<fs::PseudoDir>();
      auto compat_default_dir = fbl::MakeRefCounted<fs::PseudoDir>();
      compat_default_dir->AddEntry(
          "device", fbl::MakeRefCounted<fs::Service>([this](zx::channel server) {
            fidl::ServerEnd<fuchsia_driver_compat::Device> server_end(std::move(server));
            fidl::BindServer(dispatcher(), std::move(server_end), &test_device_);
            return ZX_OK;
          }));
      compat_dir->AddEntry("default", compat_default_dir);
      svc->AddEntry("fuchsia.driver.compat.Service", compat_dir);

      vfs_.emplace(dispatcher());
      vfs_->ServeDirectory(svc, std::move(svc_endpoints->server));
    }

    // Setup the namespace.
    fidl::Arena arena;
    fidl::VectorView<frunner::wire::ComponentNamespaceEntry> ns_entries(arena, 2);
    ns_entries[0].Allocate(arena);
    ns_entries[0].set_path(arena, "/pkg");
    ns_entries[0].set_directory(std::move(pkg_endpoints->client));
    ns_entries[1].Allocate(arena);
    ns_entries[1].set_path(arena, "/svc");
    ns_entries[1].set_directory(std::move(svc_endpoints->client));
    auto ns = driver::Namespace::Create(ns_entries);
    EXPECT_EQ(ZX_OK, ns.status_value());

    // Setup the logger.
    auto logger = driver::Logger::Create(*ns, dispatcher(), compat::Driver::Name());
    EXPECT_EQ(ZX_OK, logger.status_value());

    // Setup start args.
    fidl::VectorView<fdf::wire::NodeSymbol> symbols(arena, 1);
    symbols[0].Allocate(arena);
    symbols[0].set_name(arena, compat::kOps);
    symbols[0].set_address(arena, reinterpret_cast<uint64_t>(ops));

    fidl::VectorView<fdata::wire::DictionaryEntry> program_entries(arena, 1);
    program_entries[0].key.Set(arena, "compat");
    program_entries[0].value = fdata::wire::DictionaryValue::WithStr(arena, "driver/v1_test.so");
    fdata::wire::Dictionary program(arena);
    program.set_entries(arena, std::move(program_entries));

    fdf::wire::DriverStartArgs start_args(arena);
    start_args.set_url(arena, "fuchsia-pkg://fuchsia.com/driver#meta/driver.cm");
    start_args.set_symbols(arena, std::move(symbols));
    start_args.set_program(arena, std::move(program));
    start_args.set_outgoing_dir(std::move(outgoing_dir_endpoints->server));
    start_args.set_ns(arena, ns_entries);

    // Start driver.
    auto result = compat::Driver::Start(start_args, dispatcher_.borrow(), std::move(node),
                                        std::move(*ns), std::move(*logger));
    EXPECT_EQ(ZX_OK, result.status_value());
    return std::move(result.value());
  }

  void RunUntilDispatchersIdle() {
    bool ran = false;
    do {
      fdf_testing_wait_until_all_dispatchers_idle();
      ran = RunLoopUntilIdle();
    } while (ran);
  }

  void WaitForChildDeviceAdded() {
    while (!node().HasChildren()) {
      RunUntilDispatchersIdle();
    }
    EXPECT_TRUE(node().HasChildren());
  }

  void ShutdownDriverDispatcher() {
    dispatcher_.ShutdownAsync();
    EXPECT_EQ(ZX_OK, dispatcher_shutdown_.Wait());
  }

  TestProfileProvider profile_provider_;

 private:
  TestNode node_;
  TestRootResource root_resource_;
  TestItems items_;
  TestDevice test_device_;
  TestFile compat_file_;
  TestFile v1_test_file_;
  TestFile firmware_file_;
  TestDirectory pkg_directory_;
  TestExporter exporter_;
  std::optional<fs::ManagedVfs> vfs_;

  // This loop is for FIDL servers that get called in a sync fashion from
  // the driver.
  async::Loop fidl_loop_ = async::Loop(&kAsyncLoopConfigNoAttachToCurrentThread);

  fdf::Dispatcher dispatcher_;
  libsync::Completion dispatcher_shutdown_;
};

TEST_F(DriverTest, Start) {
  zx_protocol_device_t ops{
      .get_protocol = [](void*, uint32_t, void*) { return ZX_OK; },
  };
  auto driver = StartDriver("/pkg/driver/v1_test.so", &ops);

  // Verify that v1_test.so has added a child device.
  WaitForChildDeviceAdded();

  // Verify that v1_test.so has set a context.
  std::unique_ptr<V1Test> v1_test(static_cast<V1Test*>(driver->Context()));
  ASSERT_NE(nullptr, v1_test.get());

  // Verify v1_test.so state after bind.
  {
    const std::lock_guard<std::mutex> lock(v1_test->lock);
    EXPECT_TRUE(v1_test->did_bind);
    EXPECT_EQ(ZX_OK, v1_test->status);
    EXPECT_FALSE(v1_test->did_create);
    EXPECT_FALSE(v1_test->did_release);
  }

  // Verify v1_test.so state after release.
  ShutdownDriverDispatcher();
  driver.reset();
  ASSERT_TRUE(RunLoopUntilIdle());
  {
    const std::lock_guard<std::mutex> lock(v1_test->lock);
    EXPECT_TRUE(v1_test->did_release);
  }
}

TEST_F(DriverTest, Start_WithCreate) {
  zx_protocol_device_t ops{};
  auto driver = StartDriver("/pkg/driver/v1_create_test.so", &ops);

  // Verify that v1_test.so has added a child device.
  WaitForChildDeviceAdded();

  // Verify that v1_test.so has set a context.
  std::unique_ptr<V1Test> v1_test(static_cast<V1Test*>(driver->Context()));
  ASSERT_NE(nullptr, v1_test.get());

  // Verify v1_test.so state after bind.
  {
    const std::lock_guard<std::mutex> lock(v1_test->lock);
    EXPECT_EQ(ZX_OK, v1_test->status);
    EXPECT_FALSE(v1_test->did_bind);
    EXPECT_TRUE(v1_test->did_create);
    EXPECT_FALSE(v1_test->did_release);
  }

  // Verify v1_test.so state after release.
  ShutdownDriverDispatcher();
  driver.reset();
  ASSERT_TRUE(RunLoopUntilIdle());
  {
    const std::lock_guard<std::mutex> lock(v1_test->lock);
    EXPECT_TRUE(v1_test->did_release);
  }
}

TEST_F(DriverTest, Start_MissingBindAndCreate) {
  zx_protocol_device_t ops{};
  auto driver = StartDriver("/pkg/driver/v1_missing_test.so", &ops);

  // Verify that v1_test.so has not added a child device.
  RunUntilDispatchersIdle();
  EXPECT_FALSE(node().HasChildren());

  // Verify that v1_test.so has not set a context.
  EXPECT_EQ(nullptr, driver->Context());

  ShutdownDriverDispatcher();
}

TEST_F(DriverTest, Start_DeviceAddNull) {
  zx_protocol_device_t ops{};
  auto driver = StartDriver("/pkg/driver/v1_device_add_null_test.so", &ops);

  // Verify that v1_test.so has added a child device.
  WaitForChildDeviceAdded();

  ShutdownDriverDispatcher();
}

TEST_F(DriverTest, Start_CheckCompatService) {
  zx_protocol_device_t ops{};
  auto driver = StartDriver("/pkg/driver/v1_device_add_null_test.so", &ops);

  // Verify that v1_test.so has added a child device.
  WaitForChildDeviceAdded();

  // Check topological path.
  ASSERT_STREQ(driver->GetDevice().topological_path().data(), "/dev/test/my-device");

  // Check metadata.
  std::array<uint8_t, 3> expected_metadata;
  std::array<uint8_t, 3> metadata;
  size_t size = 0;

  ASSERT_EQ(driver->GetDevice().GetMetadata(1, metadata.data(), metadata.size(), &size), ZX_OK);
  ASSERT_EQ(size, 3ul);
  expected_metadata = {1, 2, 3};
  ASSERT_EQ(metadata, expected_metadata);

  ASSERT_EQ(driver->GetDevice().GetMetadata(2, metadata.data(), metadata.size(), &size), ZX_OK);
  ASSERT_EQ(size, 3ul);
  expected_metadata = {4, 5, 6};
  ASSERT_EQ(metadata, expected_metadata);

  ShutdownDriverDispatcher();
}

TEST_F(DriverTest, DISABLED_Start_RootResourceIsConstant) {
  // Set the root resource before the test starts.
  zx_handle_t resource;
  {
    std::scoped_lock lock(kDriverGlobalsLock);
    ASSERT_EQ(ZX_OK, zx_event_create(0, kRootResource.reset_and_get_address()));
    resource = kRootResource.get();
  }

  zx_protocol_device_t ops{};
  auto driver = StartDriver("/pkg/driver/v1_device_add_null_test.so", &ops);

  RunUntilDispatchersIdle();

  zx_handle_t resource2 = get_root_resource();

  // Check that the root resource's value did not change.
  ASSERT_EQ(resource, resource2);

  ShutdownDriverDispatcher();
}

TEST_F(DriverTest, Start_GetBackingMemory) {
  compat_file().SetStatus(ZX_ERR_UNAVAILABLE);

  zx_protocol_device_t ops{};
  auto driver = StartDriver("/pkg/driver/v1_test.so", &ops);

  // Verify that v1_test.so has not added a child device.
  RunUntilDispatchersIdle();
  EXPECT_FALSE(node().HasChildren());

  // Verify that v1_test.so has not set a context.
  EXPECT_EQ(nullptr, driver->Context());

  ShutdownDriverDispatcher();
}

TEST_F(DriverTest, Start_BindFailed) {
  zx_protocol_device_t ops{};
  auto driver = StartDriver("/pkg/driver/v1_test.so", &ops);

  // Verify that v1_test.so has set a context.
  while (!driver->Context()) {
    RunUntilDispatchersIdle();
  }
  std::unique_ptr<V1Test> v1_test(static_cast<V1Test*>(driver->Context()));
  ASSERT_NE(nullptr, v1_test.get());

  // Verify that v1_test.so has been bound.
  while (!v1_test->did_bind) {
    RunUntilDispatchersIdle();
  }

  // Verify that v1_test.so has not added a child device.
  EXPECT_FALSE(node().HasChildren());

  {
    const std::lock_guard<std::mutex> lock(v1_test->lock);
    EXPECT_TRUE(v1_test->did_bind);
    EXPECT_EQ(ZX_ERR_NOT_SUPPORTED, v1_test->status);

    EXPECT_FALSE(v1_test->did_create);
    EXPECT_FALSE(v1_test->did_release);
  }

  // Verify v1_test.so state after release.
  ShutdownDriverDispatcher();
  driver.reset();
  ASSERT_TRUE(RunLoopUntilIdle());

  {
    const std::lock_guard<std::mutex> lock(v1_test->lock);
    EXPECT_TRUE(v1_test->did_release);
  }
}

TEST_F(DriverTest, LoadFirwmareAsync) {
  zx_protocol_device_t ops{};
  auto driver = StartDriver("/pkg/driver/v1_test.so", &ops);

  // Verify that v1_test.so has set a context.
  while (!driver->Context()) {
    RunUntilDispatchersIdle();
  }
  std::unique_ptr<V1Test> v1_test(static_cast<V1Test*>(driver->Context()));
  ASSERT_NE(nullptr, v1_test.get());

  // Verify that v1_test.so has not added a child device.
  EXPECT_FALSE(node().HasChildren());

  bool was_called = false;

  driver->LoadFirmwareAsync(
      nullptr, "test",
      [](void* ctx, zx_status_t status, zx_handle_t fw, size_t size) {
        ASSERT_EQ(status, ZX_OK);
        ASSERT_EQ(size, 16ul);
        zx::vmo vmo(fw);
        auto buf = std::vector<char>(size);
        vmo.read(buf.data(), 0, size);
        buf.push_back('\0');
        ASSERT_STREQ(buf.data(), "Hello, firmware!");

        *reinterpret_cast<bool*>(ctx) = true;
      },
      &was_called);
  while (!was_called) {
    RunUntilDispatchersIdle();
  }
  ASSERT_TRUE(was_called);

  ShutdownDriverDispatcher();
  driver.reset();
  ASSERT_TRUE(RunLoopUntilIdle());
}

TEST_F(DriverTest, GetProfile) {
  profile_provider_.SetGetProfileCallback([](uint32_t priority, std::string_view name) {
    ASSERT_EQ(10u, priority);
    ASSERT_EQ("test-profile", name);
  });

  zx_protocol_device_t ops{
      .get_protocol = [](void*, uint32_t, void*) { return ZX_OK; },
  };
  auto driver = StartDriver("/pkg/driver/v1_test.so", &ops);
  // Verify that v1_test.so has added a child device.
  WaitForChildDeviceAdded();

  // Verify that v1_test.so has set a context.
  std::unique_ptr<V1Test> v1_test(static_cast<V1Test*>(driver->Context()));
  ASSERT_NE(nullptr, v1_test.get());

  // device_get_profile blocks, so we have to do it in a separate thread.
  sync_completion_t finished;
  auto thread = std::thread([&finished, &v1_test]() {
    zx_handle_t out_profile;
    ASSERT_EQ(ZX_OK, device_get_profile(v1_test->zxdev, 10, "test-profile", &out_profile));
    sync_completion_signal(&finished);
  });
  do {
    RunUntilDispatchersIdle();
  } while (sync_completion_wait(&finished, ZX_TIME_INFINITE_PAST) == ZX_ERR_TIMED_OUT);
  thread.join();

  ShutdownDriverDispatcher();
  driver.reset();
  ASSERT_TRUE(RunLoopUntilIdle());
}

TEST_F(DriverTest, GetDeadlineProfile) {
  profile_provider_.SetGetDeadlineProfileCallback(
      [](TestProfileProvider::GetDeadlineProfileRequestView& rv) {
        ASSERT_EQ(10u, rv->capacity);
        ASSERT_EQ(20u, rv->deadline);
        ASSERT_EQ(30u, rv->period);
        std::string_view sv(rv->name.data(), rv->name.size());
        ASSERT_EQ("test-profile", sv);
      });

  zx_protocol_device_t ops{
      .get_protocol = [](void*, uint32_t, void*) { return ZX_OK; },
  };
  auto driver = StartDriver("/pkg/driver/v1_test.so", &ops);
  // Verify that v1_test.so has added a child device.
  WaitForChildDeviceAdded();

  // Verify that v1_test.so has set a context.
  std::unique_ptr<V1Test> v1_test(static_cast<V1Test*>(driver->Context()));
  ASSERT_NE(nullptr, v1_test.get());

  // device_get_profile blocks, so we have to do it in a separate thread.
  sync_completion_t finished;
  auto thread = std::thread([&finished, &v1_test]() {
    zx_handle_t out_profile;
    ASSERT_EQ(ZX_OK, device_get_deadline_profile(v1_test->zxdev, 10, 20, 30, "test-profile",
                                                 &out_profile));
    sync_completion_signal(&finished);
  });
  do {
    RunUntilDispatchersIdle();
  } while (sync_completion_wait(&finished, ZX_TIME_INFINITE_PAST) == ZX_ERR_TIMED_OUT);
  thread.join();

  ShutdownDriverDispatcher();
  driver.reset();
  ASSERT_TRUE(RunLoopUntilIdle());
}
