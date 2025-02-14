// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_TESTS_FIDL_SERVER_SUITE_HARNESS_ORDINALS_H_
#define SRC_TESTS_FIDL_SERVER_SUITE_HARNESS_ORDINALS_H_

#include <cstdint>

namespace server_suite {

// To find all ordinals:
//
//     cat
//     out/default/fidling/gen/src/tests/fidl/server_suite/fidl/fidl.serversuite/llcpp/fidl/fidl.serversuite/cpp/wire_messaging.cc
//     | grep -e 'constexpr.*kTarget.*Ordinal' -A 1
//
// While using `jq` would be much nicer, large numbers are mishandled and the
// displayed ordinal ends up being incorrect.
//
// Ordinals are redefined here even though they may be accessible via C++
// binding definitions to ensure they are unchanged by changes in the bindings.
static const uint64_t kOrdinalOneWayNoPayload = 462698674125537694lu;
static const uint64_t kOrdinalTwoWayNoPayload = 6618634609655918175lu;
static const uint64_t kOrdinalTwoWayStructPayload = 3546419415198665872lu;
static const uint64_t kOrdinalTwoWayTablePayload = 7142567342575659946lu;
static const uint64_t kOrdinalTwoWayUnionPayload = 8633460217663942074lu;
static const uint64_t kOrdinalTwoWayResult = 806800322701855052lu;
static const uint64_t kOrdinalGetHandleRights = 1195943399487699944lu;
static const uint64_t kOrdinalGetSignalableEventRights = 475344252578913711lu;
static const uint64_t kOrdinalEchoAsTransferableSignalableEvent = 6829189580925709472lu;
static const uint64_t kOrdinalCloseWithEpitaph = 2952455201600597941lu;
static const uint64_t kOrdinalByteVectorSize = 1174084469162245669lu;
static const uint64_t kOrdinalHandleVectorSize = 5483915628125979959lu;
static const uint64_t kOrdinalCreateNByteVector = 2219580753158511713lu;
static const uint64_t kOrdinalCreateNHandleVector = 2752855654734922045lu;

static const uint64_t kOrdinalEpitaph = 0xffffffffffffffffu;

}  // namespace server_suite

#endif  // SRC_TESTS_FIDL_SERVER_SUITE_HARNESS_ORDINALS_H_
