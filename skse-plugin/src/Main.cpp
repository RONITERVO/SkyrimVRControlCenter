#include "PCH.h"
#include "Telemetry.h"
#include "higgsinterface001.h"
#include <atomic>
#include <spdlog/sinks/basic_file_sink.h>

namespace {
constexpr char kPluginName[] = "PSVR2SkyrimState";
// SKSEVR encodes a distribution tag in the low nibble of runtimeVersion.
// The live SKSEVR 2.0.12 interface reports 0x010400F1 (1.4.15.1), whereas
// CommonLib's RUNTIME_VR_1_4_15 is the executable file version 1.4.15.0.
// Both Query and Load must validate the SKSE interface's tagged version.
constexpr auto kSkseVrRuntime = REL::Version::unpack(0x010400F1);
static_assert(kSkseVrRuntime == REL::Version(1, 4, 15, 1));
std::atomic<bool> sessionLoaded{false};
std::atomic<bool> dataLoaded{false};
std::atomic<bool> refreshQueued{false};
std::atomic<std::uint64_t> epoch{0};
const SKSE::TaskInterface* taskInterface = nullptr;

// These are actual Skyrim menu IDs, not translated display labels. HUD,
// Loading/Main/Mist/Fader/Console and gameplay overlays deliberately excluded.
constexpr std::array<std::string_view, 13> kInteractiveMenus{
    "InventoryMenu", "MagicMenu", "ContainerMenu", "BarterMenu",
    "Dialogue Menu", "Crafting Menu", "MessageBoxMenu", "Journal Menu",
    "TweenMenu", "Book Menu", "GiftMenu", "Training Menu", "Sleep/Wait Menu"
};

void ResetSession(bool loaded) {
    ++epoch;
    sessionLoaded.store(loaded, std::memory_order_release);
    bridge::Snapshot reset{};
    reset.gameLoaded = loaded;
    bridge::Publish(reset);
}

bridge::Held ReadHeldOnMainThread(bool isLeft) {
    if (!g_higgsInterface) return bridge::Held::None;
    // HIGGS owns the object reference; never retain it across a task or send it
    // to the network thread. Do not read the pointer passed to a grab callback.
    auto* object = g_higgsInterface->GetGrabbedObject(isLeft);
    if (!object) return bridge::Held::None;
    return object->GetFormType() == RE::FormType::ActorCharacter
        ? bridge::Held::Actor : bridge::Held::Object;
}

void RefreshOnMainThread() {
    bridge::Snapshot snapshot{};
    if (!dataLoaded.load(std::memory_order_acquire) ||
        !sessionLoaded.load(std::memory_order_acquire)) {
        bridge::Publish(snapshot);
        return;
    }
    auto* ui = RE::UI::GetSingleton();
    if (!ui || ui->IsMenuOpen("Main Menu") || ui->IsMenuOpen("Loading Menu")) {
        bridge::Publish(snapshot);
        return;
    }
    snapshot.gameLoaded = true;
    // Keep every effect cleared while a noninteractive blocking UI owns input.
    if (ui->IsMenuOpen("Console") || ui->IsMenuOpen("Fader Menu") || ui->IsMenuOpen("Mist Menu")) {
        bridge::Publish(snapshot);
        return;
    }
    for (const auto menu : kInteractiveMenus) {
        if (ui->IsMenuOpen(menu)) { snapshot.menuOpen = true; break; }
    }
    snapshot.heldLeft = ReadHeldOnMainThread(true);
    snapshot.heldRight = ReadHeldOnMainThread(false);
    bridge::Publish(snapshot);
}

void QueueRefresh() {
    if (!taskInterface || refreshQueued.exchange(true, std::memory_order_acq_rel)) return;
    const auto queuedEpoch = epoch.load(std::memory_order_acquire);
    taskInterface->AddTask([queuedEpoch] {
        refreshQueued.store(false, std::memory_order_release);
        // Tasks queued before a load transition must not republish old state.
        if (queuedEpoch != epoch.load(std::memory_order_acquire)) {
            QueueRefresh();
            return;
        }
        RefreshOnMainThread();
    });
}

void OnGrabOrDrop(bool, RE::TESObjectREFR*) { QueueRefresh(); }
void OnStashOrConsume(bool, RE::TESForm*) { QueueRefresh(); }

class MenuSink final : public RE::BSTEventSink<RE::MenuOpenCloseEvent> {
public:
    RE::BSEventNotifyControl ProcessEvent(const RE::MenuOpenCloseEvent* event,
        RE::BSTEventSource<RE::MenuOpenCloseEvent>*) override {
        if (event) {
            const std::string_view name = event->menuName.c_str();
            if (event->opening && name == "Main Menu") ResetSession(false);
            if (event->opening && name == "Loading Menu") bridge::Publish({});
            QueueRefresh();
        }
        return RE::BSEventNotifyControl::kContinue;
    }
};
MenuSink menuSink;

void OnSKSEMessage(SKSE::MessagingInterface::Message* message) {
    if (!message) return;
    switch (message->type) {
    case SKSE::MessagingInterface::kPostPostLoad:
        HiggsPluginAPI::GetHiggsInterface001(SKSE::GetPluginHandle(), SKSE::GetMessagingInterface());
        if (g_higgsInterface) {
            g_higgsInterface->AddGrabbedCallback(OnGrabOrDrop);
            g_higgsInterface->AddDroppedCallback(OnGrabOrDrop);
            g_higgsInterface->AddStashedCallback(OnStashOrConsume);
            g_higgsInterface->AddConsumedCallback(OnStashOrConsume);
            SKSE::log::info("HIGGS001 connected; build {}", g_higgsInterface->GetBuildNumber());
        } else {
            SKSE::log::warn("HIGGS001 unavailable; held state remains none, menu reporting still works");
        }
        break;
    case SKSE::MessagingInterface::kDataLoaded:
        taskInterface->AddTask([] {
            if (auto* ui = RE::UI::GetSingleton()) {
                ui->AddEventSink<RE::MenuOpenCloseEvent>(&menuSink);
                dataLoaded.store(true, std::memory_order_release);
                SKSE::log::info("Menu event sink registered");
                QueueRefresh();
            }
        });
        break;
    case SKSE::MessagingInterface::kPreLoadGame:
        ResetSession(false);
        break;
    case SKSE::MessagingInterface::kPostLoadGame:
        // SKSEVR's message data is success cast to void*, not a bool pointer.
        ResetSession(message->data != nullptr);
        QueueRefresh();
        break;
    case SKSE::MessagingInterface::kNewGame:
        ResetSession(true);
        QueueRefresh();
        break;
    default: break;
    }
}

void InitializeLog() {
    const auto directory = SKSE::log::log_directory();
    if (!directory) return;
    const auto path = *directory / "PSVR2SkyrimState.log";
    auto sink = std::make_shared<spdlog::sinks::basic_file_sink_mt>(path.string(), true);
    auto logger = std::make_shared<spdlog::logger>(kPluginName, std::move(sink));
    spdlog::set_default_logger(std::move(logger));
    spdlog::set_level(spdlog::level::info);
    spdlog::flush_on(spdlog::level::info);
}
}

extern "C" __declspec(dllexport) bool SKSEPlugin_Query(const SKSE::QueryInterface* skse, SKSE::PluginInfo* info) {
    info->infoVersion = SKSE::PluginInfo::kVersion;
    info->name = kPluginName;
    info->version = 2;
    return skse && !skse->IsEditor() && skse->RuntimeVersion() == kSkseVrRuntime;
}

extern "C" __declspec(dllexport) bool SKSEPlugin_Load(const SKSE::LoadInterface* skse) {
    if (!skse || skse->IsEditor() || skse->RuntimeVersion() != kSkseVrRuntime) return false;
    try {
        InitializeLog();
        SKSE::Init(skse);
        taskInterface = SKSE::GetTaskInterface();
        const auto* messaging = SKSE::GetMessagingInterface();
        if (!taskInterface || !messaging || !messaging->RegisterListener(OnSKSEMessage)) return false;
        if (!bridge::StartTelemetry(19462, QueueRefresh)) {
            SKSE::log::error("Unable to start localhost UDP sender");
            return false;
        }
        SKSE::log::info("PSVR2SkyrimState 0.1.1 loaded; UDP 127.0.0.1:19462; no hardware output");
        return true;
    } catch (const std::exception& error) {
        SKSE::log::error("Plugin startup failed: {}", error.what());
        return false;
    }
}
