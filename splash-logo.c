/* splash-logo - blit the SplashOS boot logo onto the framebuffer.
 *
 * Reads a SPLASHLOGO2 raw asset (28-byte header: magic(12) + u32 LE width,
 * height, nframes, frame_delay_ms; then nframes frames of w*h*3 BGR bytes,
 * composited over black), hides the text console so nothing repaints over
 * it, and blits the frames centered on the screen, scaled to a fraction of
 * the screen height (Apple-boot-logo style; default 25%, override with
 * LOGO_PERCENT).
 *
 * The frames are resampled with a box/tent kernel (area averaging when
 * shrinking, bilinear when growing): point sampling a 560x531 animation
 * down to a ~200px logo drops most of the source rows and visibly aliases
 * the wave's edges.
 *
 * The animation plays ONCE and the LAST frame stays on the screen - the
 * process exits and nothing repaints until the UI or kexec takes over.
 * SPLASH_ONCE blits only the first frame (kexec blit path).
 *
 * The asset is mmapped and read frame by frame, so the ~60MB animation does
 * not have to be copied into anonymous memory before the first frame shows.
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
#include <sys/stat.h>
#include <unistd.h>
#include <linux/fb.h>

#define HDR_MAGIC	"SPLASHLOGO2"
#define HDR_SIZE	28
#define DEFAULT_LOGO	"/usr/share/splashos/boot-logo.raw"
#define DEFAULT_PERCENT	25
#define BPP		3	/* BGR24 */

/* One axis of the resampling kernel: destination pixel d takes the source
 * texels [first, first+n), each with a weight in 1/65536 units. */
struct span {
	unsigned first;
	unsigned n;
	unsigned *weight;
};

static struct span *spans_build(unsigned src, unsigned dst)
{
	struct span *sp = calloc(dst, sizeof(*sp));
	if (!sp)
		return NULL;

	for (unsigned d = 0; d < dst; d++) {
		/* source interval covered by destination pixel d, 16.16 fixed point */
		unsigned long long lo = (unsigned long long)d * src * 65536 / dst;
		unsigned long long hi = (unsigned long long)(d + 1) * src * 65536 / dst;
		unsigned first = (unsigned)(lo >> 16);
		unsigned last = (unsigned)((hi + 65535) >> 16);
		unsigned n;

		if (last > src)
			last = src;
		n = last - first;
		sp[d].weight = malloc((size_t)n * sizeof(unsigned));
		if (!sp[d].weight)
			return NULL;
		sp[d].first = first;
		sp[d].n = n;
		for (unsigned i = 0; i < n; i++) {
			unsigned long long s = (unsigned long long)(first + i) << 16;
			unsigned long long e = s + 65536;
			sp[d].weight[i] = (unsigned)((hi < e ? hi : e)
						   - (lo > s ? lo : s));
		}
	}
	return sp;
}

static void spans_free(struct span *sp, unsigned dst)
{
	if (!sp)
		return;
	for (unsigned d = 0; d < dst; d++)
		free(sp[d].weight);
	free(sp);
}

static void blit_frame(const unsigned char *src, unsigned sw,
		       unsigned char *fb, size_t scanline,
		       unsigned xoff, unsigned yoff,
		       const struct span *xs, const struct span *ys,
		       unsigned tw, unsigned th)
{
	for (unsigned y = 0; y < th; y++) {
		unsigned char *drow = fb + (size_t)(yoff + y) * scanline
				      + (size_t)xoff * 4;
		const struct span *spy = &ys[y];

		for (unsigned x = 0; x < tw; x++) {
			const struct span *spx = &xs[x];
			unsigned long long sb = 0, sg = 0, sr = 0, sw_sum = 0;
			unsigned char *d = drow + (size_t)x * 4;

			for (unsigned j = 0; j < spy->n; j++) {
				const unsigned char *srow = src
					+ (size_t)(spy->first + j) * sw * BPP;
				unsigned long long wy = spy->weight[j];

				for (unsigned i = 0; i < spx->n; i++) {
					const unsigned char *p = srow
						+ (size_t)(spx->first + i) * BPP;
					unsigned long long w = wy * spx->weight[i];

					sb += w * p[0];
					sg += w * p[1];
					sr += w * p[2];
					sw_sum += w;
				}
			}
			d[0] = (unsigned char)((sb + sw_sum / 2) / sw_sum);
			d[1] = (unsigned char)((sg + sw_sum / 2) / sw_sum);
			d[2] = (unsigned char)((sr + sw_sum / 2) / sw_sum);
			d[3] = 0;	/* x8r8g8b8: padding byte */
		}
	}
}

int main(int argc, char **argv)
{
	const char *fbdev = getenv("FB0") ? : "/dev/fb0";
	const char *logo = getenv("LOGO") ? : DEFAULT_LOGO;
	int percent = getenv("LOGO_PERCENT") ? atoi(getenv("LOGO_PERCENT"))
					     : DEFAULT_PERCENT;
	if (argc > 1)
		logo = argv[1];

	/* ---- asset: validate the header before touching the screen ---- */
	int lfd = open(logo, O_RDONLY);
	if (lfd < 0)
		return 1;
	struct stat lst;
	if (fstat(lfd, &lst) < 0 || lst.st_size < HDR_SIZE) {
		close(lfd);
		return 1;
	}
	const unsigned char *asset = mmap(NULL, (size_t)lst.st_size, PROT_READ,
					  MAP_PRIVATE, lfd, 0);
	if (asset == MAP_FAILED) {
		close(lfd);
		return 1;
	}

	if (memcmp(asset, HDR_MAGIC, strlen(HDR_MAGIC)) != 0) {
		munmap((void *)asset, (size_t)lst.st_size);
		close(lfd);
		return 1;
	}
	unsigned w, h, nframes, delay_ms;
	memcpy(&w, asset + 12, 4);
	memcpy(&h, asset + 16, 4);
	memcpy(&nframes, asset + 20, 4);
	memcpy(&delay_ms, asset + 24, 4);
	size_t fsize = (size_t)w * h * BPP;
	if (!w || !h || !nframes ||
	    fsize / BPP / h != w ||
	    (unsigned long long)HDR_SIZE + (unsigned long long)fsize * nframes
					> (unsigned long long)lst.st_size) {
		munmap((void *)asset, (size_t)lst.st_size);
		close(lfd);
		return 1;
	}

	/* ---- framebuffer ---- */
	int fd = open(fbdev, O_RDWR);
	if (fd < 0) {
		munmap((void *)asset, (size_t)lst.st_size);
		close(lfd);
		return 1;
	}

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
		munmap((void *)asset, (size_t)lst.st_size);
		close(lfd);
		return 1;
	}
	size_t scanline = (size_t)vinfo.xres_virtual * 4;	/* 32bpp: 4 bytes/pixel */
	size_t screen = scanline * vinfo.yres_virtual;

	/* logo geometry: scaled to <percent> of the screen height (kept
	 * within the screen bounds), centered. */
	if (percent <= 0 || percent > 100)
		percent = DEFAULT_PERCENT;
	unsigned th = (unsigned)vinfo.yres * (unsigned)percent / 100u;
	unsigned tw = (unsigned)((unsigned long long)w * th / h);
	if (!tw || !th || tw > (unsigned)vinfo.xres) {
		tw = vinfo.xres;
		th = (unsigned)((unsigned long long)h * tw / w);
	}
	unsigned xoff = (vinfo.xres > tw) ? ((unsigned)vinfo.xres - tw) / 2u : 0;
	unsigned yoff = (vinfo.yres > th) ? ((unsigned)vinfo.yres - th) / 2u : 0;

	/* resampling kernels (same for every frame) */
	struct span *xs = spans_build(w, tw);
	struct span *ys = spans_build(h, th);
	if (!xs || !ys) {
		spans_free(xs, tw);
		spans_free(ys, th);
		close(fd);
		munmap((void *)asset, (size_t)lst.st_size);
		close(lfd);
		return 1;
	}

	/* hide the text console so fbcon does not repaint over the logo */
	int vtc = open("/sys/class/vtconsole/vtcon1/bind", O_WRONLY);
	if (vtc >= 0) {
		(void)!write(vtc, "0", 1);
		close(vtc);
	}

	unsigned char *fb = mmap(NULL, screen, PROT_WRITE, MAP_SHARED, fd, 0);
	if (fb == MAP_FAILED) {
		spans_free(xs, tw);
		spans_free(ys, th);
		close(fd);
		munmap((void *)asset, (size_t)lst.st_size);
		close(lfd);
		return 1;
	}
	memset(fb, 0, screen);		/* black background */

	/* Apple-style: play the animation ONCE and hold the LAST frame.
	 * The loop exits after the final frame and the framebuffer keeps it
	 * until something else repaints (the UI or kexec). SPLASH_ONCE
	 * blits only the first frame (kexec blit path) and exits. */
	int once = getenv("SPLASH_ONCE") != NULL;

	for (unsigned f = 0; f < nframes; f++) {
		blit_frame(asset + HDR_SIZE + (size_t)f * fsize, w, fb, scanline,
			   xoff, yoff, xs, ys, tw, th);
		if (once || f + 1 == nframes)
			break;
		if (delay_ms)
			usleep(delay_ms * 1000u);
	}

	munmap(fb, screen);
	spans_free(xs, tw);
	spans_free(ys, th);
	close(fd);
	munmap((void *)asset, (size_t)lst.st_size);
	close(lfd);
	return 0;
}
