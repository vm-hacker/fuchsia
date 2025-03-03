// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/console/commands/verb_run_component.h"

#include "src/developer/debug/zxdb/client/remote_api.h"
#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/target.h"
#include "src/developer/debug/zxdb/common/string_util.h"
#include "src/developer/debug/zxdb/console/command.h"
#include "src/developer/debug/zxdb/console/console.h"
#include "src/developer/debug/zxdb/console/output_buffer.h"
#include "src/developer/debug/zxdb/console/string_util.h"
#include "src/developer/debug/zxdb/console/verbs.h"

namespace zxdb {
namespace {

const char kShortHelp[] = "run-component: Run the component.";
const char kHelp[] =
    R"(run-component <url> [ <args>* ]

  Runs the component with the given URL.

  V2 components will be launched in the "ffx-laboratory" collection, similar to
  the behavior of "ffx component run --recreate". The collection only provides
  a restricted set of capabilities and is only suitable for running some demo
  components. If any other capabilities are needed, it's recommended to declare
  it statically and attach to it from the debugger.

  See https://fuchsia.dev/fuchsia-src/development/components/run#ffx-laboratory.

Arguments

  <url>
      The URL of the component to run. Both v1 and v2 components are supported.
      v1 components have their URLs ending with ".cmx", while v2 components have
      their URLs ending with ".cm".

  <args>*

      Extra arguments when launching the component, only supported in v1
      components.

Examples

  run-component fuchsia-pkg://fuchsia.com/crasher#meta/cpp_crasher.cmx log_fatal
  run-component fuchsia-pkg://fuchsia.com/crasher#meta/cpp_crasher.cm
)";

Err Exec(ConsoleContext* context, const Command& cmd, CommandCallback callback) {
  // No nouns should be provided.
  if (Err err = cmd.ValidateNouns({}); err.has_error()) {
    return err;
  }

  if (cmd.args().empty()) {
    return Err("No component to run. Try \"run-component <url>\".");
  }

  if (StringEndsWith(cmd.args()[0], ".cm")) {
    // Output warning about this possibly not working.
    OutputBuffer warning(Syntax::kWarning, GetExclamation());
    warning.Append(
        " run-component won't work for many v2 components. See \"help run-component\".\n");
    Console::get()->Output(warning);
  }

  // Launch the component.
  debug_ipc::LaunchRequest request;
  request.inferior_type = debug_ipc::InferiorType::kComponent;
  request.argv = cmd.args();

  cmd.target()->session()->remote_api()->Launch(
      request, [cb = std::move(callback)](Err err, debug_ipc::LaunchReply reply) mutable {
        if (!err.has_error() && reply.status.has_error()) {
          err = Err("Could not start component: %s", reply.status.message().c_str());
        }
        if (err.has_error()) {
          Console::get()->Output(err);
        }
        if (cb) {
          cb(err);
        }
      });

  return Err();
}

}  // namespace

VerbRecord GetRunComponentVerbRecord() {
  return {&Exec, {"run-component"}, kShortHelp, kHelp, CommandGroup::kProcess};
}

}  // namespace zxdb
