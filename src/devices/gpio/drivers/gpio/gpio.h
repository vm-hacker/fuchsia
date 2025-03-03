// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_GPIO_DRIVERS_GPIO_GPIO_H_
#define SRC_DEVICES_GPIO_DRIVERS_GPIO_GPIO_H_

#include <fidl/fuchsia.hardware.gpio/cpp/wire.h>
#include <fuchsia/hardware/gpio/cpp/banjo.h>
#include <fuchsia/hardware/gpioimpl/cpp/banjo.h>
#include <lib/ddk/platform-defs.h>
#include <lib/zircon-internal/thread_annotations.h>

#include <string_view>

#include <ddktl/device.h>
#include <ddktl/fidl.h>
#include <fbl/mutex.h>

namespace gpio {

class GpioDevice;
using fuchsia_hardware_gpio::Gpio;
using fuchsia_hardware_gpio::wire::GpioFlags;
using GpioDeviceType =
    ddk::Device<GpioDevice, ddk::Messageable<Gpio>::Mixin, ddk::Openable, ddk::Closable>;

static_assert(GPIO_PULL_DOWN == static_cast<uint32_t>(GpioFlags::kPullDown),
              "ConfigIn PULL_DOWN flag doesn't match.");
static_assert(GPIO_PULL_UP == static_cast<uint32_t>(GpioFlags::kPullUp),
              "ConfigIn PULL_UP flag doesn't match.");
static_assert(GPIO_NO_PULL == static_cast<uint32_t>(GpioFlags::kNoPull),
              "ConfigIn NO_PULL flag doesn't match.");
static_assert(GPIO_PULL_MASK == static_cast<uint32_t>(GpioFlags::kPullMask),
              "ConfigIn PULL_MASK flag doesn't match.");

class GpioDevice : public GpioDeviceType, public ddk::GpioProtocol<GpioDevice, ddk::base_protocol> {
 public:
  GpioDevice(zx_device_t* parent, gpio_impl_protocol_t* gpio, uint32_t pin, std::string_view name)
      : GpioDeviceType(parent), gpio_(gpio), pin_(pin), name_(name) {}

  static zx_status_t Create(void* ctx, zx_device_t* parent);

  void DdkRelease();
  zx_status_t DdkOpen(zx_device_t** dev_out, uint32_t flags);
  zx_status_t DdkClose(uint32_t flags);

  zx_status_t GpioGetPin(uint32_t* pin);
  zx_status_t GpioGetName(char* out_name);
  zx_status_t GpioConfigIn(uint32_t flags);
  zx_status_t GpioConfigOut(uint8_t initial_value);
  zx_status_t GpioSetAltFunction(uint64_t function);
  zx_status_t GpioRead(uint8_t* out_value);
  zx_status_t GpioWrite(uint8_t value);
  zx_status_t GpioGetInterrupt(uint32_t flags, zx::interrupt* out_irq);
  zx_status_t GpioReleaseInterrupt();
  zx_status_t GpioSetPolarity(gpio_polarity_t polarity);
  zx_status_t GpioSetDriveStrength(uint64_t ds_ua, uint64_t* out_actual_ds_ua);
  zx_status_t GpioGetDriveStrength(uint64_t* ds_ua);

  // FIDL
  void GetPin(GetPinRequestView request, GetPinCompleter::Sync& completer) override {
    completer.ReplySuccess(pin_);
  }
  void GetName(GetNameRequestView request, GetNameCompleter::Sync& completer) override {
    completer.ReplySuccess(::fidl::StringView::FromExternal(name_));
  }
  void ConfigIn(ConfigInRequestView request, ConfigInCompleter::Sync& completer) override {
    zx_status_t status = GpioConfigIn(static_cast<uint32_t>(request->flags));
    if (status == ZX_OK) {
      completer.ReplySuccess();
    } else {
      completer.ReplyError(status);
    }
  }
  void ConfigOut(ConfigOutRequestView request, ConfigOutCompleter::Sync& completer) override {
    zx_status_t status = GpioConfigOut(request->initial_value);
    if (status == ZX_OK) {
      completer.ReplySuccess();
    } else {
      completer.ReplyError(status);
    }
  }
  void Read(ReadRequestView request, ReadCompleter::Sync& completer) override {
    uint8_t value = 0;
    zx_status_t status = GpioRead(&value);
    if (status == ZX_OK) {
      completer.ReplySuccess(value);
    } else {
      completer.ReplyError(status);
    }
  }
  void Write(WriteRequestView request, WriteCompleter::Sync& completer) override {
    zx_status_t status = GpioWrite(request->value);
    if (status == ZX_OK) {
      completer.ReplySuccess();
    } else {
      completer.ReplyError(status);
    }
  }
  void SetDriveStrength(SetDriveStrengthRequestView request,
                        SetDriveStrengthCompleter::Sync& completer) override {
    uint64_t actual = 0;
    zx_status_t status = GpioSetDriveStrength(request->ds_ua, &actual);
    if (status == ZX_OK) {
      completer.ReplySuccess(actual);
    } else {
      completer.ReplyError(status);
    }
  }
  void GetDriveStrength(GetDriveStrengthRequestView request,
                        GetDriveStrengthCompleter::Sync& completer) override {
    uint64_t result_ua = 0;
    zx_status_t status = GpioGetDriveStrength(&result_ua);
    if (status == ZX_OK) {
      completer.ReplySuccess(result_ua);
    } else {
      completer.ReplyError(status);
    }
  }
  void GetInterrupt(GetInterruptRequestView request,
                    GetInterruptCompleter::Sync& completer) override {
    zx::interrupt interrupt;
    zx_status_t status = GpioGetInterrupt(request->flags, &interrupt);
    if (status == ZX_OK) {
      completer.ReplySuccess(std::move(interrupt));
    } else {
      completer.ReplyError(status);
    }
  }
  void ReleaseInterrupt(ReleaseInterruptRequestView request,
                        ReleaseInterruptCompleter::Sync& completer) override {
    zx_status_t status = GpioReleaseInterrupt();
    if (status == ZX_OK) {
      completer.ReplySuccess();
    } else {
      completer.ReplyError(status);
    }
  }

 private:
  const ddk::GpioImplProtocolClient gpio_ TA_GUARDED(lock_);
  const uint32_t pin_;
  const std::string name_;
  fbl::Mutex lock_;
  bool opened_ TA_GUARDED(lock_) = false;
};

}  // namespace gpio

#endif  // SRC_DEVICES_GPIO_DRIVERS_GPIO_GPIO_H_
