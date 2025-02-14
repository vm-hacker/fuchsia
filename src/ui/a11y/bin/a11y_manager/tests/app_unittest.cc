// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/a11y/bin/a11y_manager/app.h"

#include <fuchsia/accessibility/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/sys/cpp/testing/component_context_provider.h>
#include <lib/syslog/cpp/macros.h>

#include <gtest/gtest.h>

#include "src/lib/testing/loop_fixture/test_loop_fixture.h"
#include "src/ui/a11y/bin/a11y_manager/tests/mocks/mock_color_transform_handler.h"
#include "src/ui/a11y/bin/a11y_manager/tests/mocks/mock_focus_chain.h"
#include "src/ui/a11y/bin/a11y_manager/tests/mocks/mock_pointer_event_registry.h"
#include "src/ui/a11y/bin/a11y_manager/tests/mocks/mock_property_provider.h"
#include "src/ui/a11y/bin/a11y_manager/tests/mocks/mock_setui_accessibility.h"
#include "src/ui/a11y/bin/a11y_manager/tests/util/util.h"
#include "src/ui/a11y/lib/annotation/tests/mocks/mock_annotation_view.h"
#include "src/ui/a11y/lib/gesture_manager/recognizers/one_finger_n_tap_recognizer.h"
#include "src/ui/a11y/lib/magnifier/tests/mocks/mock_magnification_handler.h"
#include "src/ui/a11y/lib/screen_reader/tests/mocks/mock_screen_reader_context.h"
#include "src/ui/a11y/lib/screen_reader/tests/mocks/mock_tts_engine.h"
#include "src/ui/a11y/lib/semantics/tests/mocks/mock_semantic_listener.h"
#include "src/ui/a11y/lib/semantics/tests/mocks/mock_semantic_provider.h"
#include "src/ui/a11y/lib/semantics/tests/mocks/mock_semantics_event_manager.h"
#include "src/ui/a11y/lib/testing/input.h"
#include "src/ui/a11y/lib/util/tests/mocks/mock_boot_info_manager.h"
#include "src/ui/a11y/lib/util/util.h"
#include "src/ui/a11y/lib/view/a11y_view_semantics.h"
#include "src/ui/a11y/lib/view/tests/mocks/mock_accessibility_view.h"
#include "src/ui/a11y/lib/view/tests/mocks/mock_view_injector_factory.h"

namespace accessibility_test {
namespace {

using fuchsia::accessibility::semantics::Node;
using fuchsia::accessibility::semantics::NodePtr;
using fuchsia::ui::input::accessibility::EventHandling;
using fuchsia::ui::input::accessibility::PointerEventListener;
using fuchsia::ui::input::accessibility::PointerEventListenerPtr;

class AppUnitTest : public gtest::TestLoopFixture {
 public:
  AppUnitTest()
      : context_provider_(),
        context_(context_provider_.context()),
        mock_pointer_event_registry_(&context_provider_),
        mock_color_transform_handler_(&context_provider_),
        mock_setui_(&context_provider_),
        mock_focus_chain_(&context_provider_),
        mock_property_provider_(&context_provider_),
        mock_annotation_view_factory_(new MockAnnotationViewFactory()),
        mock_boot_info_manager_(context_, true),
        view_manager_(std::make_unique<a11y::SemanticTreeServiceFactory>(),
                      std::make_unique<a11y::A11yViewSemanticsFactory>(),
                      std::unique_ptr<MockAnnotationViewFactory>(mock_annotation_view_factory_),
                      std::make_unique<MockViewInjectorFactory>(),
                      std::make_unique<MockSemanticsEventManager>(),
                      std::make_unique<MockAccessibilityView>(), context_provider_.context()),
        tts_manager_(context_),
        color_transform_manager_(context_),
        mock_semantic_provider_(&view_manager_),
        screen_reader_context_factory_() {}

  void AddNodeToTree(uint32_t node_id, std::string label,
                     std::vector<uint32_t> child_ids = std::vector<uint32_t>()) {
    std::vector<fuchsia::accessibility::semantics::Node> updates;
    updates.emplace_back(CreateTestNode(node_id, label, child_ids));
    mock_semantic_provider_.UpdateSemanticNodes(std::move(updates));
    mock_semantic_provider_.CommitUpdates();
    RunLoopUntilIdle();
  }

  // Sends pointer events and returns the |handled| argument of the (last) resulting
  // |OnStreamHandled| invocation.
  //
  // Yo dawg, I heard you like pointer event listener pointers, so I took a pointer to your pointer
  // event listener pointer so you can receive events while you receive events (while honoring the
  // C++ style guide).
  std::optional<EventHandling> SendPointerEvents(PointerEventListenerPtr* listener,
                                                 const std::vector<PointerParams>& events) {
    std::optional<EventHandling> event_handling;
    listener->events().OnStreamHandled = [&event_handling](uint32_t /*unused*/, uint32_t /*unused*/,
                                                           EventHandling handled) {
      event_handling = handled;
    };

    for (const auto& params : events) {
      SendPointerEvent(listener->get(), params);
      if (event_handling == fuchsia::ui::input::accessibility::EventHandling::REJECTED) {
        break;
      }
    }

    return event_handling;
  }

  void SendPointerEvent(PointerEventListener* listener, const PointerParams& params) {
    listener->OnEvent(ToPointerEvent(params, input_event_time_++, mock_semantic_provider_.koid()));

    // Simulate trivial passage of time (can expose edge cases with posted async tasks).
    RunLoopUntilIdle();
  }

  // Sends a gesture that wouldn't be recognized by any accessibility feature, for testing arena
  // configuration.
  std::optional<EventHandling> SendUnrecognizedGesture(PointerEventListenerPtr* listener) {
    return SendPointerEvents(listener, Zip({TapEvents(1, {}), TapEvents(2, {})}));
  }

  void ConnectSpeakerAndEngineToTtsManager() {
    // The speaker and engine need to be connected to the tts manager before the
    // screen reader announces it's on. In order to verify that the screen reader
    // correctly vocalizes, we need to expicitly connect the speaker and engine.
    fuchsia::accessibility::tts::EnginePtr engine_ptr;
    tts_manager_.OpenEngine(
        engine_ptr.NewRequest(),
        [](fuchsia::accessibility::tts::TtsManager_OpenEngine_Result result) {});
    RunLoopUntilIdle();

    MockTtsEngine mock_tts_engine;
    tts_manager_.RegisterEngine(
        mock_tts_engine.GetHandle(),
        [](fuchsia::accessibility::tts::EngineRegistry_RegisterEngine_Result result) {});
    RunLoopUntilIdle();
  }

  std::unique_ptr<a11y_manager::App> GetApp() {
    auto app = std::make_unique<a11y_manager::App>(
        context_, &view_manager_, &tts_manager_, &color_transform_manager_,
        &gesture_listener_registry_, &mock_boot_info_manager_, &screen_reader_context_factory_);

    RunLoopUntilIdle();
    // App is created, but is not fully-initialized.  Make sure the fetch of settings only happens
    // after it has been initialized.
    EXPECT_EQ(0, mock_setui_.num_watch_called());
    // Right now, obtaining the locale causes the app to be fully-initialized.
    EXPECT_EQ(1, mock_property_provider_.get_profile_count());
    mock_property_provider_.SetLocale("en");
    mock_property_provider_.ReplyToGetProfile();
    RunLoopUntilIdle();
    EXPECT_EQ(1,
              mock_property_provider_.get_profile_count());  // Stil 1, no changes in profile yet.
    // Note: 2 here because as soon as we get a settings, we call Watch() again.
    EXPECT_EQ(2, mock_setui_.num_watch_called());

    return app;
  }

  sys::testing::ComponentContextProvider context_provider_;
  sys::ComponentContext* context_;

  MockPointerEventRegistry mock_pointer_event_registry_;
  MockColorTransformHandler mock_color_transform_handler_;
  MockSetUIAccessibility mock_setui_;
  MockFocusChain mock_focus_chain_;
  MockPropertyProvider mock_property_provider_;
  MockAnnotationViewFactory* mock_annotation_view_factory_;
  MockBootInfoManager mock_boot_info_manager_;
  a11y::ViewManager view_manager_;
  a11y::TtsManager tts_manager_;
  a11y::ColorTransformManager color_transform_manager_;
  a11y::GestureListenerRegistry gesture_listener_registry_;
  MockSemanticProvider mock_semantic_provider_;
  MockScreenReaderContextFactory screen_reader_context_factory_;

 private:
  // We don't actually use these times. If we did, we'd want to more closely correlate them with
  // fake time.
  uint64_t input_event_time_ = 0;
};

// Test to make sure ViewManager Service is exposed by A11y.
// Test sends a node update to ViewManager and then compare the expected
// result using log file created by semantics manager.
TEST_F(AppUnitTest, UpdateNodeToSemanticsManager) {
  auto app = GetApp();
  // Turn on the screen reader.
  fuchsia::settings::AccessibilitySettings settings;
  settings.set_screen_reader(true);
  mock_setui_.Set(std::move(settings), [](auto) {});

  // Enble semantics and verify that they were enabled correctly.
  view_manager_.SetSemanticsEnabled(true);
  RunLoopUntilIdle();
  EXPECT_TRUE(mock_semantic_provider_.GetSemanticsEnabled());

  // Creating test node to update.
  AddNodeToTree(0, "Label A");

  // Check that the node is in the semantic tree
  auto created_node = view_manager_.GetSemanticNode(mock_semantic_provider_.koid(), 0u);
  EXPECT_TRUE(created_node);
  EXPECT_EQ(created_node->attributes().label(), "Label A");
}

// This test makes sure that services implemented by the Tts manager are
// available.
TEST_F(AppUnitTest, OffersTtsManagerServices) {
  auto app = GetApp();
  fuchsia::accessibility::tts::TtsManagerPtr tts_manager;
  context_provider_.ConnectToPublicService(tts_manager.NewRequest());
  RunLoopUntilIdle();
  EXPECT_TRUE(tts_manager.is_bound());
}

TEST_F(AppUnitTest, NoListenerInitially) {
  auto app = GetApp();
  mock_setui_.Set({}, [](auto) {});

  RunLoopUntilIdle();
  EXPECT_FALSE(mock_pointer_event_registry_.listener())
      << "No listener should be registered in the beginning, as there is no accessibility service "
         "enabled.";
}

TEST_F(AppUnitTest, ListenerForScreenReader) {
  auto app = GetApp();
  EXPECT_FALSE(app->state().screen_reader_enabled());

  fuchsia::settings::AccessibilitySettings settings;
  settings.set_screen_reader(true);
  mock_setui_.Set(std::move(settings), [](auto) {});

  RunLoopUntilIdle();
  EXPECT_TRUE(app->state().screen_reader_enabled());

  ASSERT_TRUE(mock_pointer_event_registry_.listener());
  EXPECT_EQ(SendUnrecognizedGesture(&mock_pointer_event_registry_.listener()),
            EventHandling::CONSUMED);
}

TEST_F(AppUnitTest, ListenerForMagnifier) {
  auto app = GetApp();
  fuchsia::settings::AccessibilitySettings settings;
  settings.set_enable_magnification(true);
  mock_setui_.Set(std::move(settings), [](auto) {});

  RunLoopUntilIdle();
  EXPECT_TRUE(app->state().magnifier_enabled());

  ASSERT_TRUE(mock_pointer_event_registry_.listener());
  EXPECT_EQ(SendUnrecognizedGesture(&mock_pointer_event_registry_.listener()),
            EventHandling::REJECTED);
}

TEST_F(AppUnitTest, ListenerForAll) {
  auto app = GetApp();
  fuchsia::settings::AccessibilitySettings settings;
  settings.set_screen_reader(true);
  settings.set_enable_magnification(true);
  mock_setui_.Set(std::move(settings), [](auto) {});

  RunLoopUntilIdle();
  ASSERT_TRUE(mock_pointer_event_registry_.listener());
  EXPECT_EQ(SendUnrecognizedGesture(&mock_pointer_event_registry_.listener()),
            EventHandling::CONSUMED);
}

TEST_F(AppUnitTest, NoListenerAfterAllRemoved) {
  auto app = GetApp();
  {
    fuchsia::settings::AccessibilitySettings settings;
    settings.set_screen_reader(true);
    settings.set_enable_magnification(true);
    mock_setui_.Set(std::move(settings), [](auto) {});
  }
  RunLoopUntilIdle();
  {
    fuchsia::settings::AccessibilitySettings settings;
    settings.set_screen_reader(false);
    settings.set_enable_magnification(false);
    mock_setui_.Set(std::move(settings), [](auto) {});
  }

  RunLoopUntilIdle();
  EXPECT_FALSE(mock_pointer_event_registry_.listener());
}

// Covers a couple additional edge cases around removing listeners.
TEST_F(AppUnitTest, ListenerRemoveOneByOne) {
  auto app = GetApp();
  {
    fuchsia::settings::AccessibilitySettings settings;
    settings.set_screen_reader(true);
    settings.set_enable_magnification(true);
    mock_setui_.Set(std::move(settings), [](auto) {});
  }
  RunLoopUntilIdle();
  {
    fuchsia::settings::AccessibilitySettings settings;
    settings.set_screen_reader(false);
    settings.set_enable_magnification(true);
    mock_setui_.Set(std::move(settings), [](auto) {});
  }
  RunLoopUntilIdle();

  EXPECT_EQ(app->state().screen_reader_enabled(), false);
  EXPECT_EQ(app->state().magnifier_enabled(), true);

  ASSERT_TRUE(mock_pointer_event_registry_.listener());
  EXPECT_EQ(SendUnrecognizedGesture(&mock_pointer_event_registry_.listener()),
            EventHandling::REJECTED);
  {
    fuchsia::settings::AccessibilitySettings settings;
    settings.set_screen_reader(false);
    settings.set_enable_magnification(false);
    mock_setui_.Set(std::move(settings), [](auto) {});
  }
  RunLoopUntilIdle();

  EXPECT_EQ(app->state().magnifier_enabled(), false);
  EXPECT_FALSE(mock_pointer_event_registry_.listener());
}

// Makes sure gesture priorities are right. If they're not, screen reader would intercept this
// gesture.
TEST_F(AppUnitTest, MagnifierGestureWithScreenReader) {
  auto app = GetApp();
  MockMagnificationHandler mag_handler;
  fidl::Binding<fuchsia::accessibility::MagnificationHandler> mag_handler_binding(&mag_handler);
  {
    fuchsia::accessibility::MagnifierPtr magnifier;
    context_provider_.ConnectToPublicService(magnifier.NewRequest());
    magnifier->RegisterHandler(mag_handler_binding.NewBinding());
  }

  fuchsia::settings::AccessibilitySettings settings;
  settings.set_screen_reader(true);
  settings.set_enable_magnification(true);
  mock_setui_.Set(std::move(settings), [](auto) {});
  RunLoopUntilIdle();

  SendPointerEvents(&mock_pointer_event_registry_.listener(), 3 * TapEvents(1, {}));
  RunLoopFor(a11y::Magnifier2::kTransitionPeriod);

  EXPECT_GT(mag_handler.transform().scale, 1);
}

TEST_F(AppUnitTest, ColorCorrectionApplied) {
  auto app = GetApp();
  fuchsia::settings::AccessibilitySettings accessibilitySettings;
  accessibilitySettings.set_screen_reader(false);
  accessibilitySettings.set_color_inversion(false);
  accessibilitySettings.set_enable_magnification(false);
  accessibilitySettings.set_color_correction(fuchsia::settings::ColorBlindnessType::NONE);
  mock_setui_.Set(std::move(accessibilitySettings), [](auto) {});

  RunLoopUntilIdle();

  EXPECT_EQ(fuchsia::accessibility::ColorCorrectionMode::DISABLED,
            mock_color_transform_handler_.GetColorCorrectionMode());

  // Turn on color correction.
  fuchsia::settings::AccessibilitySettings newAccessibilitySettings;
  newAccessibilitySettings.set_color_correction(
      fuchsia::settings::ColorBlindnessType::DEUTERANOMALY);
  mock_setui_.Set(std::move(newAccessibilitySettings), [](auto) {});
  RunLoopUntilIdle();

  // Verify that stuff changed
  EXPECT_EQ(fuchsia::accessibility::ColorCorrectionMode::CORRECT_DEUTERANOMALY,
            mock_color_transform_handler_.GetColorCorrectionMode());
}

TEST_F(AppUnitTest, ColorInversionApplied) {
  auto app = GetApp();
  fuchsia::settings::AccessibilitySettings accessibilitySettings;
  accessibilitySettings.set_screen_reader(false);
  accessibilitySettings.set_color_inversion(false);
  accessibilitySettings.set_enable_magnification(false);
  accessibilitySettings.set_color_correction(fuchsia::settings::ColorBlindnessType::NONE);
  mock_setui_.Set(std::move(accessibilitySettings), [](auto) {});
  RunLoopUntilIdle();

  EXPECT_FALSE(mock_color_transform_handler_.GetColorInversionEnabled());

  // Turn on color correction.
  fuchsia::settings::AccessibilitySettings newAccessibilitySettings;
  newAccessibilitySettings.set_color_inversion(true);
  mock_setui_.Set(std::move(newAccessibilitySettings), [](auto) {});
  RunLoopUntilIdle();

  // Verify that stuff changed
  EXPECT_TRUE(mock_color_transform_handler_.GetColorInversionEnabled());
}

TEST_F(AppUnitTest, ScreenReaderOnAtUserInitiatedReboot) {
  fuchsia::settings::AccessibilitySettings accessibilitySettings;
  accessibilitySettings.set_screen_reader(true);
  accessibilitySettings.set_color_inversion(false);
  accessibilitySettings.set_enable_magnification(false);
  accessibilitySettings.set_color_correction(fuchsia::settings::ColorBlindnessType::NONE);

  mock_setui_.Set(std::move(accessibilitySettings), [](auto) {});
  RunLoopUntilIdle();

  auto app = GetApp();

  // Verify that screen reader is on and the pointer event registry is wired up.
  EXPECT_TRUE(app->state().screen_reader_enabled());
  ASSERT_TRUE(mock_pointer_event_registry_.listener());
  EXPECT_EQ(SendUnrecognizedGesture(&mock_pointer_event_registry_.listener()),
            EventHandling::CONSUMED);

  // Both a speaker and an engine need to be connected to the TTS manager in
  // order for the screen reader to announce that it's on.
  ConnectSpeakerAndEngineToTtsManager();

  auto mock_screen_reader_context = screen_reader_context_factory_.mock_screen_reader_context();
  ASSERT_TRUE(mock_screen_reader_context);

  auto mock_speaker = mock_screen_reader_context->mock_speaker_ptr();
  ASSERT_TRUE(mock_speaker);
  EXPECT_TRUE(mock_speaker->ReceivedSpeak());
  EXPECT_EQ(mock_speaker->message_ids().size(), 1u);
}

TEST_F(AppUnitTest, ScreenReaderOnAtSystemReboot) {
  fuchsia::settings::AccessibilitySettings accessibilitySettings;
  accessibilitySettings.set_screen_reader(true);
  accessibilitySettings.set_color_inversion(false);
  accessibilitySettings.set_enable_magnification(false);
  accessibilitySettings.set_color_correction(fuchsia::settings::ColorBlindnessType::NONE);

  mock_setui_.Set(std::move(accessibilitySettings), [](auto) {});
  RunLoopUntilIdle();

  mock_boot_info_manager_.SetLastRebootWasUserInitiated(false);
  auto app = GetApp();

  // Both a speaker and an engine need to be connected to the TTS manager in
  // order for the screen reader to announce that it's on.
  ConnectSpeakerAndEngineToTtsManager();

  auto mock_screen_reader_context = screen_reader_context_factory_.mock_screen_reader_context();
  ASSERT_TRUE(mock_screen_reader_context);

  auto mock_speaker = mock_screen_reader_context->mock_speaker_ptr();
  ASSERT_TRUE(mock_speaker);
  EXPECT_FALSE(mock_speaker->ReceivedSpeak());
}

TEST_F(AppUnitTest, ScreenReaderAnnouncesOnWhenUserExplicitlyEnables) {
  {
    fuchsia::settings::AccessibilitySettings accessibilitySettings;
    accessibilitySettings.set_screen_reader(false);
    accessibilitySettings.set_color_inversion(false);
    accessibilitySettings.set_enable_magnification(false);
    accessibilitySettings.set_color_correction(fuchsia::settings::ColorBlindnessType::NONE);

    mock_setui_.Set(std::move(accessibilitySettings), [](auto) {});
    RunLoopUntilIdle();
  }

  auto app = GetApp();

  auto mock_screen_reader_context = screen_reader_context_factory_.mock_screen_reader_context();
  // Screen reader should not yet be instantiated. Since the context is created
  // at the same time as the screen reader object, we can check whether the
  // context has been initialized.
  ASSERT_FALSE(mock_screen_reader_context);

  {
    fuchsia::settings::AccessibilitySettings accessibilitySettings;
    accessibilitySettings.set_screen_reader(true);
    accessibilitySettings.set_color_inversion(false);
    accessibilitySettings.set_enable_magnification(false);
    accessibilitySettings.set_color_correction(fuchsia::settings::ColorBlindnessType::NONE);

    mock_setui_.Set(std::move(accessibilitySettings), [](auto) {});
    RunLoopUntilIdle();
  }

  // Both a speaker and an engine need to be connected to the TTS manager in
  // order for the screen reader to announce that it's on.
  ConnectSpeakerAndEngineToTtsManager();

  mock_screen_reader_context = screen_reader_context_factory_.mock_screen_reader_context();
  ASSERT_TRUE(mock_screen_reader_context);

  auto mock_speaker = mock_screen_reader_context->mock_speaker_ptr();
  ASSERT_TRUE(mock_speaker);
  EXPECT_TRUE(mock_speaker->ReceivedSpeak());
  EXPECT_EQ(mock_speaker->message_ids().size(), 1u);
}

TEST_F(AppUnitTest, InitializesFocusChain) {
  auto app = GetApp();
  // Ensures that when App is initialized, it connects to the Focus Chain different services.
  RunLoopUntilIdle();

  ASSERT_TRUE(mock_focus_chain_.listener());
}

// Makes sure FocusChain is wired up with the screen reader, when screen reader is enabled.
// This test uses explore action to make sure when a node is tapped, then screen reader can call
// RequestFocus() on FocusChain. This confirms that FocusChain is connected to ScreenReader.
TEST_F(AppUnitTest, FocusChainIsWiredToScreenReader) {
  auto app = GetApp();
  // Enable Screen Reader.
  fuchsia::settings::AccessibilitySettings accessibilitySettings;
  accessibilitySettings.set_screen_reader(true);
  accessibilitySettings.set_color_inversion(false);
  accessibilitySettings.set_enable_magnification(false);
  accessibilitySettings.set_color_correction(fuchsia::settings::ColorBlindnessType::NONE);
  mock_setui_.Set(std::move(accessibilitySettings), [](auto) {});
  RunLoopUntilIdle();

  // Creating test node to update.
  AddNodeToTree(0, "Label A");

  auto created_node = view_manager_.GetSemanticNode(mock_semantic_provider_.koid(), 0u);
  EXPECT_TRUE(created_node);
  EXPECT_EQ(created_node->attributes().label(), "Label A");

  // Set HitTest result which is required to know which node is being tapped.
  mock_semantic_provider_.SetHitTestResult(0);

  // Send Tap event for view_ref_. This should trigger explore action, which should then call
  // FocusChain to set focus to the tapped view.
  SendPointerEvents(&mock_pointer_event_registry_.listener(), TapEvents(1, {}));
  RunLoopFor(a11y::kMaxTapDuration);

  auto mock_screen_reader_context = screen_reader_context_factory_.mock_screen_reader_context();
  ASSERT_TRUE(mock_screen_reader_context);

  auto mock_focus_manager = mock_screen_reader_context->mock_a11y_focus_manager_ptr();
  ASSERT_TRUE(mock_focus_manager);

  auto a11y_focus = mock_focus_manager->GetA11yFocus();
  ASSERT_TRUE(a11y_focus);

  EXPECT_EQ(mock_semantic_provider_.koid(), a11y_focus->view_ref_koid);
}

TEST_F(AppUnitTest, FetchesLocaleInfoOnStartup) {
  auto app = GetApp();
  // App is initialized, so it should have requested once the locale.
  ASSERT_EQ(1, mock_property_provider_.get_profile_count());
  mock_property_provider_.SetLocale("en-US");
  mock_property_provider_.SendOnChangeEvent();
  RunLoopUntilIdle();
  // The event causes GetProfile() to be invoked again from the a11y manager side. Check if the call
  // happened through the mock.
  ASSERT_EQ(2, mock_property_provider_.get_profile_count());
}

TEST_F(AppUnitTest, ScreenReaderReinitializesWhenLocaleChanges) {
  auto app = GetApp();
  fuchsia::settings::AccessibilitySettings accessibilitySettings;
  accessibilitySettings.set_screen_reader(true);
  accessibilitySettings.set_color_inversion(false);
  accessibilitySettings.set_enable_magnification(false);
  accessibilitySettings.set_color_correction(fuchsia::settings::ColorBlindnessType::NONE);
  mock_setui_.Set(std::move(accessibilitySettings), [](auto) {});
  RunLoopUntilIdle();
  EXPECT_TRUE(app->state().screen_reader_enabled());
  auto old_screen_reader_ptr = app->screen_reader();
  ASSERT_TRUE(old_screen_reader_ptr);
  EXPECT_EQ(old_screen_reader_ptr->context()->locale_id(), "en");
  mock_property_provider_.SetLocale("en-US");
  mock_property_provider_.SendOnChangeEvent();
  RunLoopUntilIdle();
  // The event causes GetProfile() to be invoked again from the a11y manager side. Check if the call
  // happened through the mock.
  ASSERT_EQ(2, mock_property_provider_.get_profile_count());
  // Sends a reply.
  mock_property_provider_.ReplyToGetProfile();
  RunLoopUntilIdle();
  auto new_screen_reader_ptr = app->screen_reader();
  ASSERT_TRUE(new_screen_reader_ptr);
  EXPECT_EQ(new_screen_reader_ptr->context()->locale_id(), "en-US");
  EXPECT_NE(new_screen_reader_ptr, old_screen_reader_ptr);
}

TEST_F(AppUnitTest, ScreenReaderUsesDefaultLocaleIfPropertyProviderDisconnectsOrIsNotAvailable) {
  auto app = GetApp();
  EXPECT_FALSE(app->state().screen_reader_enabled());
  mock_property_provider_.CloseChannels();
  RunLoopUntilIdle();
  // Only one call to GetProfile happened, because the channel was closed.
  ASSERT_EQ(1, mock_property_provider_.get_profile_count());
  // Turns on the Screen Reader and checks that it picks up the default locale.
  fuchsia::settings::AccessibilitySettings accessibilitySettings;
  accessibilitySettings.set_screen_reader(true);
  accessibilitySettings.set_color_inversion(false);
  accessibilitySettings.set_enable_magnification(false);
  accessibilitySettings.set_color_correction(fuchsia::settings::ColorBlindnessType::NONE);
  mock_setui_.Set(std::move(accessibilitySettings), [](auto) {});
  RunLoopUntilIdle();
  EXPECT_EQ(app->screen_reader()->context()->locale_id(), "en-US");
}

TEST_F(AppUnitTest, OffersVirtualkeyboardServices) {
  auto app = GetApp();
  fuchsia::accessibility::virtualkeyboard::RegistryPtr registry;
  context_provider_.ConnectToPublicService(registry.NewRequest());
  RunLoopUntilIdle();
  EXPECT_TRUE(registry.is_bound());
}

// TODO(fxbug.dev/49924): Improve tests to cover what happens if services aren't available at
// startup.

}  // namespace
}  // namespace accessibility_test
