/* Stub used when vulkan/shaderc are unavailable: always reports
   "unsupported" so callers fall back to the CPU decoder. */
#include <stddef.h>
#include <stdint.h>
int gpujpeg_probe(const uint8_t* d, size_t n, int* w, int* h, int* dri) {
    (void)d; (void)n; (void)w; (void)h; (void)dri;
    return 1;
}
int gpujpeg_decode(const uint8_t* d, size_t n, uint8_t* o, int rr) {
    (void)d; (void)n; (void)o; (void)rr;
    return 1;
}
