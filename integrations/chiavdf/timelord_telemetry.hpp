#pragma once
#include <chrono>

namespace timelord_telemetry {
// Keep the usual active polling cadence, but never let idle boards go silent.
class Poll {
public:
    using Clock = std::chrono::steady_clock;
    explicit Poll(Clock::time_point now = Clock::now()) : last_(now) {}
    bool due(bool scheduled, Clock::time_point now = Clock::now()) {
        if (!scheduled && now - last_ < std::chrono::seconds(5)) return false;
        last_ = now;
        return true;
    }
private:
    Clock::time_point last_;
};
}
