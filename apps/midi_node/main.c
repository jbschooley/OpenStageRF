
#include "radio.h"
#include "midi.h"

int main(void) {
    radio_init();
    midi_init();

    while (1) {
        packet_t pkt;
        if (radio_receive(&pkt)) {
            midi_send_packet(&pkt);
        }
    }
}
