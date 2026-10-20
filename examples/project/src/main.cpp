// A small embedded-style project, without IDE clutter.
#include "device.h"
#include "sensor.hpp"
#include <array>
#include <cstdint>

namespace demo {

struct Reading {
    std::uint32_t sequence;
    float temperature;
    bool valid;
};

Reading sample(std::uint32_t sequence) {
    // Keep the data path simple and easy to inspect.
    return {sequence, sensor_temperature(), true};
}

} // namespace demo

int main() {
    const std::array readings = {
        demo::sample(1),
        demo::sample(2),
    };
    return readings[0].valid && board_ready ? 0 : 1;
}
