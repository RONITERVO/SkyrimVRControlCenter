#include "Telemetry.h"
#include <chrono>
#include <thread>

int main() {
    if (!bridge::StartTelemetry(19463)) return 1;
    using namespace std::chrono_literals;
    std::this_thread::sleep_for(1300ms);
    bridge::Publish({true, bridge::Held::None, bridge::Held::None, true});
    std::this_thread::sleep_for(1300ms);
    bridge::Publish({false, bridge::Held::Object, bridge::Held::Actor, true});
    // No refresh here: verify the worker clears stale cached game state.
    std::this_thread::sleep_for(4300ms);
    bridge::Publish({true, bridge::Held::Actor, bridge::Held::Object, true});
    std::this_thread::sleep_for(1300ms);
    bridge::Publish({});
    std::this_thread::sleep_for(1300ms);
    return 0;
}
