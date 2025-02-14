// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/crash_reports/snapshot_manager.h"

#include <lib/async/cpp/task.h>
#include <lib/fpromise/bridge.h>
#include <lib/syslog/cpp/macros.h>

#include <algorithm>
#include <fstream>
#include <memory>
#include <vector>

#include "src/developer/forensics/crash_reports/constants.h"
#include "src/developer/forensics/crash_reports/errors.h"
#include "src/developer/forensics/feedback/annotations/annotation_manager.h"
#include "src/developer/forensics/feedback/annotations/decode.h"
#include "src/lib/uuid/uuid.h"

namespace forensics {
namespace crash_reports {
namespace {

using fuchsia::feedback::Annotation;
using fuchsia::feedback::GetSnapshotParameters;

template <typename V>
void AddAnnotation(const std::string& key, const V& value, feedback::Annotations& annotations) {
  annotations.insert({key, std::to_string(value)});
}

template <>
void AddAnnotation<std::string>(const std::string& key, const std::string& value,
                                feedback::Annotations& annotations) {
  annotations.insert({key, value});
}

// Helper function to make a shared_ptr from a rvalue-reference of a type.
template <typename T>
std::shared_ptr<T> MakeShared(T&& t) {
  return std::make_shared<T>(static_cast<std::remove_reference_t<T>&&>(t));
}

}  // namespace

SnapshotManager::SnapshotManager(async_dispatcher_t* dispatcher, timekeeper::Clock* clock,
                                 feedback_data::DataProviderInternal* data_provider,
                                 feedback::AnnotationManager* annotation_manager,
                                 zx::duration shared_request_window,
                                 const std::string& garbage_collected_snapshots_path,
                                 StorageSize max_annotations_size, StorageSize max_archives_size)
    : dispatcher_(dispatcher),
      clock_(clock),
      data_provider_(data_provider),
      annotation_manager_(annotation_manager),
      shared_request_window_(shared_request_window),
      garbage_collected_snapshots_path_(garbage_collected_snapshots_path),
      max_annotations_size_(max_annotations_size),
      current_annotations_size_(0u),
      max_archives_size_(max_archives_size),
      current_archives_size_(0u),
      garbage_collected_snapshot_(kGarbageCollectedSnapshotUuid,
                                  feedback::Annotations({
                                      {"debug.snapshot.error", "garbage collected"},
                                      {"debug.snapshot.present", "false"},
                                  })),
      not_persisted_snapshot_(kNotPersistedSnapshotUuid,
                              feedback::Annotations({
                                  {"debug.snapshot.error", "not persisted"},
                                  {"debug.snapshot.present", "false"},
                              })),
      timed_out_snapshot_(kTimedOutSnapshotUuid, feedback::Annotations({
                                                     {"debug.snapshot.error", "timeout"},
                                                     {"debug.snapshot.present", "false"},
                                                 })),
      shutdown_snapshot_(kShutdownSnapshotUuid, feedback::Annotations({
                                                    {"debug.snapshot.error", "system shutdown"},
                                                    {"debug.snapshot.present", "false"},
                                                })),
      no_uuid_snapshot_(kNoUuidSnapshotUuid, feedback::Annotations({
                                                 {"debug.snapshot.error", "missing uuid"},
                                                 {"debug.snapshot.present", "false"},
                                             })) {
  // Load the file lines into a set of UUIDs.
  std::ifstream file(garbage_collected_snapshots_path_);
  for (std::string uuid; getline(file, uuid);) {
    garbage_collected_snapshots_.insert(uuid);
  }
}

Snapshot SnapshotManager::GetSnapshot(const SnapshotUuid& uuid) {
  auto BuildMissing = [this](const SpecialCaseSnapshot& special_case) {
    return MissingSnapshot(annotation_manager_->ImmediatelyAvailable(), special_case.annotations);
  };

  if (uuid == kGarbageCollectedSnapshotUuid) {
    return BuildMissing(garbage_collected_snapshot_);
  }

  if (uuid == kNotPersistedSnapshotUuid) {
    return BuildMissing(not_persisted_snapshot_);
  }

  if (uuid == kTimedOutSnapshotUuid) {
    return BuildMissing(timed_out_snapshot_);
  }

  if (uuid == kShutdownSnapshotUuid) {
    return BuildMissing(shutdown_snapshot_);
  }

  if (uuid == kNoUuidSnapshotUuid) {
    return BuildMissing(no_uuid_snapshot_);
  }

  auto* data = FindSnapshotData(uuid);

  if (!data) {
    if (garbage_collected_snapshots_.find(uuid) != garbage_collected_snapshots_.end()) {
      return BuildMissing(garbage_collected_snapshot_);
    } else {
      return BuildMissing(not_persisted_snapshot_);
    }
  }

  return ManagedSnapshot(data->annotations, data->presence_annotations, data->archive);
}

::fpromise::promise<SnapshotUuid> SnapshotManager::GetSnapshotUuid(zx::duration timeout) {
  const zx::time current_time{clock_->Now()};

  SnapshotUuid uuid;

  if (UseLatestRequest()) {
    uuid = requests_.back()->uuid;
  } else {
    uuid = MakeNewSnapshotRequest(current_time, timeout);
  }

  auto* data = FindSnapshotData(uuid);
  FX_CHECK(data);

  data->num_clients_with_uuid += 1;

  const zx::time deadline = current_time + timeout;

  // The snapshot for |uuid| may not be ready, so the logic for returning |uuid| to the client
  // needs to be wrapped in an asynchronous task that can be re-executed when the conditions for
  // returning a UUID are met, e.g., the snapshot for |uuid| is received from |data_provider_| or
  // the call to GetSnapshotUuid times out.
  return ::fpromise::make_promise(
      [this, uuid, deadline](::fpromise::context& context) -> ::fpromise::result<SnapshotUuid> {
        if (shutdown_) {
          return ::fpromise::ok(kShutdownSnapshotUuid);
        }

        auto request = FindSnapshotRequest(uuid);

        // The request and its data were deleted before the promise executed. This should only occur
        // if a snapshot is dropped immediately after it is received because its annotations and
        // archive are too large and it is one of the oldest in the FIFO.
        if (!request) {
          return ::fpromise::ok(kGarbageCollectedSnapshotUuid);
        }

        if (!request->is_pending) {
          return ::fpromise::ok(request->uuid);
        }

        if (clock_->Now() >= deadline) {
          return ::fpromise::ok(kTimedOutSnapshotUuid);
        }

        WaitForSnapshot(uuid, deadline, context.suspend_task());
        return ::fpromise::pending();
      });
}

void SnapshotManager::Release(const SnapshotUuid& uuid) {
  if (uuid == kGarbageCollectedSnapshotUuid || uuid == kNotPersistedSnapshotUuid ||
      uuid == kTimedOutSnapshotUuid || uuid == kNoUuidSnapshotUuid) {
    return;
  }

  auto* data = FindSnapshotData(uuid);

  // The snapshot was likely dropped due to size constraints.
  if (!data) {
    return;
  }

  data->num_clients_with_uuid -= 1;

  // There are still clients that need the snapshot.
  if (data->num_clients_with_uuid > 0) {
    return;
  }

  DropAnnotations(data);
  DropArchive(data);

  // No calls to GetUuid should be blocked.
  if (auto request = FindSnapshotRequest(uuid); request) {
    FX_CHECK(request->blocked_promises.empty());
  }

  requests_.erase(std::remove_if(
      requests_.begin(), requests_.end(),
      [uuid](const std::unique_ptr<SnapshotRequest>& request) { return uuid == request->uuid; }));
  RecordAsGarbageCollected(uuid);
  data_.erase(uuid);
}

void SnapshotManager::Shutdown() {
  // Unblock all pending promises to return |shutdown_snapshot_|.
  shutdown_ = true;
  for (auto& request : requests_) {
    if (!request->is_pending) {
      continue;
    }

    for (auto& blocked_promise : request->blocked_promises) {
      if (blocked_promise) {
        blocked_promise.resume_task();
      }
    }
    request->blocked_promises.clear();
  }
}

SnapshotUuid SnapshotManager::MakeNewSnapshotRequest(const zx::time start_time,
                                                     const zx::duration timeout) {
  const auto uuid = uuid::Generate();
  requests_.emplace_back(std::unique_ptr<SnapshotRequest>(new SnapshotRequest{
      .uuid = uuid,
      .is_pending = true,
      .blocked_promises = {},
      .delayed_get_snapshot = async::TaskClosure(),
  }));
  data_.emplace(uuid, SnapshotData{
                          .num_clients_with_uuid = 0,
                          .annotations_size = StorageSize::Bytes(0u),
                          .archive_size = StorageSize::Bytes(0u),
                          .annotations = nullptr,
                          .archive = nullptr,
                          .presence_annotations = nullptr,
                      });

  requests_.back()->delayed_get_snapshot.set_handler([this, timeout, uuid]() {
    // Give 15s for the packaging of the snapshot and the round-trip between the client and
    // the server and the rest is given to each data collection.
    zx::duration collection_timeout_per_data = timeout - zx::sec(15);
    data_provider_->GetSnapshotInternal(
        collection_timeout_per_data,
        [this, uuid](feedback::Annotations annotations, fuchsia::feedback::Attachment archive) {
          CompleteWithSnapshot(uuid, std::move(annotations), std::move(archive));
          EnforceSizeLimits();
        });
  });
  requests_.back()->delayed_get_snapshot.PostForTime(dispatcher_,
                                                     start_time + shared_request_window_);

  return uuid;
}

void SnapshotManager::WaitForSnapshot(const SnapshotUuid& uuid, zx::time deadline,
                                      ::fpromise::suspended_task get_uuid_promise) {
  auto* request = FindSnapshotRequest(uuid);
  if (!request) {
    get_uuid_promise.resume_task();
    return;
  }

  request->blocked_promises.push_back(std::move(get_uuid_promise));
  const size_t idx = request->blocked_promises.size() - 1;

  // Resume |get_uuid_promise| after |deadline| has passed.
  if (const zx_status_t status = async::PostTaskForTime(
          dispatcher_,
          [this, idx, uuid] {
            if (auto* request = FindSnapshotRequest(uuid); request && request->is_pending) {
              FX_CHECK(idx < request->blocked_promises.size());
              if (request->blocked_promises[idx]) {
                request->blocked_promises[idx].resume_task();
              }
            }
          },
          deadline);
      status != ZX_OK) {
    FX_PLOGS(ERROR, status) << "Failed to post async task";

    // Immediately resume the promise if posting the task fails.
    request->blocked_promises.back().resume_task();
    request->blocked_promises.pop_back();
  }
}

void SnapshotManager::CompleteWithSnapshot(const SnapshotUuid& uuid,
                                           feedback::Annotations annotations,
                                           fuchsia::feedback::Attachment archive) {
  auto* request = FindSnapshotRequest(uuid);
  auto* data = FindSnapshotData(uuid);

  // A pending request shouldn't be deleted.
  FX_CHECK(request);
  FX_CHECK(data);
  FX_CHECK(request->is_pending);

  data->presence_annotations = std::make_shared<feedback::Annotations>();

  // Add annotations about the snapshot. These are not "presence" annotations because
  // they're unchanging and not the result of the SnapshotManager's data management.
  AddAnnotation("debug.snapshot.shared-request.num-clients", data->num_clients_with_uuid,
                annotations);
  AddAnnotation("debug.snapshot.shared-request.uuid", request->uuid, annotations);

  // Take ownership of |annotations| and then record the size of the annotations and archive.
  data->annotations = MakeShared(std::move(annotations));

  for (const auto& [k, v] : *data->annotations) {
    data->annotations_size += StorageSize::Bytes(k.size());
    if (v.HasValue()) {
      data->annotations_size += StorageSize::Bytes(v.Value().size());
    }
  }
  current_annotations_size_ += data->annotations_size;

  if (!archive.key.empty() && archive.value.vmo.is_valid()) {
    data->archive = MakeShared(ManagedSnapshot::Archive(archive));

    data->archive_size += StorageSize::Bytes(data->archive->key.size());
    data->archive_size += StorageSize::Bytes(data->archive->value.size());
    current_archives_size_ += data->archive_size;
  }

  if (data->archive == nullptr) {
    data->presence_annotations->insert({"debug.snapshot.present", "false"});
  }

  // The request is completed and unblock all promises that need |annotations| and |archive|.
  request->is_pending = false;
  for (auto& blocked_promise : request->blocked_promises) {
    if (blocked_promise) {
      blocked_promise.resume_task();
    }
  }
  request->blocked_promises.clear();
}

void SnapshotManager::EnforceSizeLimits() {
  std::vector<std::unique_ptr<SnapshotRequest>> surviving_requests;
  for (auto& request : requests_) {
    // If the request is pending or the size limits aren't exceeded, keep the request.
    if (request->is_pending || (current_annotations_size_ <= max_annotations_size_ &&
                                current_archives_size_ <= max_archives_size_)) {
      surviving_requests.push_back(std::move(request));

      // Continue in order to keep the rest of the requests alive.
      continue;
    }

    auto* data = FindSnapshotData(request->uuid);
    FX_CHECK(data);

    // Drop |request|'s annotations and attachments if necessary. Attachments are dropped because
    // they don't make sense without the accompanying annotations.
    if (current_annotations_size_ > max_annotations_size_) {
      DropAnnotations(data);
      DropArchive(data);
      RecordAsGarbageCollected(request->uuid);
    }

    // Drop |request|'s archive if necessary.
    if (current_archives_size_ > max_archives_size_) {
      DropArchive(data);
      RecordAsGarbageCollected(request->uuid);
    }

    // Delete the SnapshotRequest and SnapshotData if the annotations and archive have been
    // dropped, either in this iteration of the loop or a prior one.
    if (!data->annotations && !data->archive) {
      RecordAsGarbageCollected(request->uuid);
      data_.erase(request->uuid);
      continue;
    }

    surviving_requests.push_back(std::move(request));
  }

  requests_.swap(surviving_requests);
}

void SnapshotManager::DropAnnotations(SnapshotData* data) {
  data->annotations = nullptr;
  data->presence_annotations = nullptr;

  current_annotations_size_ -= data->annotations_size;
  data->annotations_size = StorageSize::Bytes(0u);
}

void SnapshotManager::DropArchive(SnapshotData* data) {
  data->archive = nullptr;

  current_archives_size_ -= data->archive_size;
  data->archive_size = StorageSize::Bytes(0u);

  // If annotations still exist, add an annotation indicating the archive was garbage collected.
  if (data->annotations) {
    for (const auto& [k, v] : garbage_collected_snapshot_.annotations) {
      data->presence_annotations->insert({k, v});
      data->annotations_size += StorageSize::Bytes(k.size());
      current_annotations_size_ += StorageSize::Bytes(k.size());

      if (v.HasValue()) {
        data->annotations_size += StorageSize::Bytes(v.Value().size());
        current_annotations_size_ += StorageSize::Bytes(v.Value().size());
      }
    }
  }
}

void SnapshotManager::RecordAsGarbageCollected(const SnapshotUuid& uuid) {
  if (garbage_collected_snapshots_.find(uuid) != garbage_collected_snapshots_.end()) {
    return;
  }

  garbage_collected_snapshots_.insert(uuid);

  // Append the UUID to the file on its own line.
  std::ofstream file(garbage_collected_snapshots_path_, std::ofstream::out | std::ofstream::app);
  file << uuid << "\n";
  file.close();
}

bool SnapshotManager::UseLatestRequest() const {
  if (requests_.empty()) {
    return false;
  }

  // Whether the FIDL call for the latest request has already been made or not. If it has, the
  // snapshot might not contain all the logs up until now for instance so it's better to create a
  // new request.
  return requests_.back()->delayed_get_snapshot.is_pending();
}

SnapshotManager::SnapshotRequest* SnapshotManager::FindSnapshotRequest(const SnapshotUuid& uuid) {
  auto request = std::find_if(
      requests_.begin(), requests_.end(),
      [uuid](const std::unique_ptr<SnapshotRequest>& request) { return uuid == request->uuid; });
  return (request == requests_.end()) ? nullptr : request->get();
}

SnapshotManager::SnapshotData* SnapshotManager::FindSnapshotData(const SnapshotUuid& uuid) {
  return (data_.find(uuid) == data_.end()) ? nullptr : &(data_.at(uuid));
}

}  // namespace crash_reports
}  // namespace forensics
