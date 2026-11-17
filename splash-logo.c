/* splash-logo - blit the SplashOS boot logo onto the framebuffer.
 *
 * Reads a SPLASHLOGO1 raw asset (16-byte header: magic(12) + u32 LE width,
 * height, nframes, frame_delay_ms; then nframes frames of w*h*4 BGRA bytes),
 * hides the text console so nothing repaints over it, and blits the frames
 * centered on the screen. Static assets have nframes == 1: one blit, the
 * screen keeps the frame until kexec replaces it.
 *
 * The kernel framebuffer is expected to be 32bpp x8r8g8b8 (virtio-gpu
 * default). FB0 overrides the framebuffer device (testing); LOGO overrides
 * the asset path. Exits 1 without touching the screen when anything is not
 * supported, so callers can fall back to the text console silently.
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>
#include <linux/fb.h>

#define HDR_MAGIC	"SPLASHLOGO1"
#define HDR_SIZE	28
#define DEFAULT_LOGO	"/usr/share/splashos/boot-logo.raw"
#define FB_BYTES(pp)	((pp) / 8)

int main(int argc, char **argv)
{
	const char *fbdev = getenv("FB0") ? : "/dev/fb0";
	const char *logo = getenv("LOGO") ? : DEFAULT_LOGO;
	if (argc > 1) logo = argv[1];

	FILE *lf = fopen(logo, "rb");
	if (!lf) return 1;

	unsigned char hdr[HDR_SIZE];
	if (fread(hdr, 1, HDR_SIZE, lf) != HDR_SIZE ||
	    memcmp(hdr, HDR_MAGIC, strlen(HDR_MAGIC)) != 0) {
		fclose(lf);
		return 1;
	}
	unsigned w, h, nframes, delay_ms;
	memcpy(&w, hdr + 12, 4); memcpy(&h, hdr + 16, 4);
	memcpy(&nframes, hdr + 20, 4); memcpy(&delay_ms, hdr + 24, 4);
	if (!w || !h || !nframes) { fclose(lf); return 1; }
	size_t fsize = (size_t)w * h * 4;
	unsigned char *frames = malloc(fsize * nframes);
	if (!frames || fread(frames, fsize, nframes, lf) != nframes) {
		free(frames); fclose(lf); return 1;
	}
	fclose(lf);

	int fd = open(fbdev, O_RDWR);
	if (fd < 0) return 1;

	struct fb_var_screeninfo vinfo;
	memset(&vinfo, 0, sizeof(vinfo));
	if (getenv("SPLASH_TEST_XRES")) {
		/* test hook: fake geometry instead of probing the device */
		vinfo.bits_per_pixel = 32;
		vinfo.xres = vinfo.xres_virtual = atoi(getenv("SPLASH_TEST_XRES"));
		vinfo.yres = vinfo.yres_virtual = atoi(getenv("SPLASH_TEST_YRES"));
	} else if (ioctl(fd, FBIOGET_VSCREENINFO, &vinfo) < 0 ||
	    vinfo.bits_per_pixel != 32) {
		fprintf(stderr, "splash-logo: unsupported framebuffer (%dbpp)\n",
			vinfo.bits_per_pixel);
		close(fd);
		return 1;
	}
	size_t scanline = (size_t)vinfo.xres_virtual * 4;	/* 32bpp: 4 bytes/pixel */
	size_t screen = scanline * vinfo.yres_virtual;

	/* hide the text console so fbcon does not repaint over the logo */
	int vtc = open("/sys/class/vtconsole/vtcon1/bind", O_WRONLY);
	if (vtc >= 0) {
		(void)!write(vtc, "0", 1);
		close(vtc);
	}

	unsigned char *fb = mmap(NULL, screen, PROT_WRITE, MAP_SHARED, fd, 0);
	if (fb == MAP_FAILED) { close(fd); return 1; }
	memset(fb, 0, screen);		/* black background */

	size_t xoff = (vinfo.xres > w) ? (vinfo.xres - w) / 2 * 4 : 0;
	size_t yoff = (vinfo.yres > h) ? (vinfo.yres - h) / 2 : 0;
	size_t blit_w = (vinfo.xres < w) ? vinfo.xres : w;

	for (unsigned f = 0; ; f = (f + 1) % nframes) {
		unsigned char *src = frames + (size_t)f * fsize;
		for (unsigned y = 0; y < h && yoff + y < vinfo.yres; y++) {
			unsigned char *dst = fb + (yoff + y) * scanline + xoff;
			size_t bytes = blit_w * 4;
			if (xoff + bytes > scanline)
				bytes = scanline - xoff;
			memcpy(dst, src + (size_t)y * fsize / h, bytes);
		}
		if (nframes == 1)
			break;
		if (delay_ms)
			usleep(delay_ms * 1000u);
	}

	munmap(fb, screen);
	close(fd);
	return 0;
}
