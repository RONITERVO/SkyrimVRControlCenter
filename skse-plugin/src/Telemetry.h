#pragma once
#include <cstdint>

namespace bridge {
enum class Held : std::uint8_t { None, Object, Actor };

struct Snapshot {
    bool menuOpen{false};
    Held heldLeft{Held::None};
    Held heldRight{Held::None};
    bool gameLoaded{false};
    bool operator==(const Snapshot&) const = default;
};

// Process-lifetime singleton. Never calls Skyrim, OpenVR, OpenXR, or PSVR2 CAPI.
// The production endpoint is fixed to loopback:19462. Tests use 19463.
bool StartTelemetry(std::uint16_t port = 19462, void (*requestRefresh)() = nullptr) noexcept;
void Publish(const Snapshot& snapshot) noexcept;
}
