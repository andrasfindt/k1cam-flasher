/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * Dumps the T31 cloner data k1cam-flasher embeds, using thingino-dfu's own
 * code, so the embedded bytes are exactly what that tool puts on the wire.
 *
 *   ddr_t31x.bin    DDR configuration for the T31X family (T31X/N/L/ZL/ZC/LC)
 *   marker.bin      the 172-byte "ILOP" partition marker
 *   chips.bin       per SPI NOR chip: name[32] jedec(u32 le) size(u32 le)
 *                   write descriptor[972]  (the read descriptor is the same
 *                   with the erase flag at 0xC8+0x14 cleared)
 *
 * See gen.sh.
 */
#include "tdfu/tdfu.h"
#include "tdfu/flash_descriptor.h"
#include "flash/spi_nor_db.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

bool g_debug_enabled = false;

static void put32(uint8_t *p, uint32_t v)
{
	p[0] = v;
	p[1] = v >> 8;
	p[2] = v >> 16;
	p[3] = v >> 24;
}

int main(int argc, char **argv)
{
	char path[512];
	firmware_files_t fw;

	if (argc != 3 || firmware_load_from_dir(TDFU_VARIANT_T31X, argv[1], &fw) != TDFU_SUCCESS) {
		fprintf(stderr, "usage: gen <thingino-dfu firmware dir> <out dir>\n");
		return 1;
	}
	const char *out = argv[2];
	snprintf(path, sizeof(path), "%s/ddr_t31x.bin", out);
	FILE *f = fopen(path, "wb");
	fwrite(fw.config, 1, fw.config_size, f);
	fclose(f);

	uint8_t full[FLASH_DESCRIPTOR_SIZE];
	flash_descriptor_create_t31x_writer_full(full);
	snprintf(path, sizeof(path), "%s/marker.bin", out);
	f = fopen(path, "wb");
	fwrite(full + 0x1c, 1, 0xac, f);
	fclose(f);

	size_t n;
	const spi_nor_chip_t *chips = spi_nor_list(&n);
	snprintf(path, sizeof(path), "%s/chips.bin", out);
	f = fopen(path, "wb");
	for (size_t i = 0; i < n; i++) {
		uint8_t rec[32 + 8 + FLASH_DESCRIPTOR_SIZE] = {0};
		strncpy((char *)rec, chips[i].name, 31);
		put32(rec + 32, chips[i].jedec_id);
		put32(rec + 36, chips[i].size);
		flash_descriptor_create(&chips[i], rec + 40);
		fwrite(rec, 1, sizeof(rec), f);
	}
	fclose(f);
	fprintf(stderr, "ddr %zu bytes, %zu chips\n", fw.config_size, n);
	return 0;
}
