# EXIF container fixture provenance

The five `exif.*` files in this directory are unchanged copies of upstream test
containers from **kamadak-exif 0.6.1**, by KAMADA Ken'ichi. They were copied from the
published crates.io package, whose `.cargo_vcs_info.json` identifies commit
`ba531e6bb523bf7c01849cf8e679beff4ef960b0`.

- [Upstream repository](https://github.com/kamadak/exif-rs)
- [Original test directory at the recorded commit](https://github.com/kamadak/exif-rs/tree/ba531e6bb523bf7c01849cf8e679beff4ef960b0/tests)
- [Published package version](https://crates.io/crates/kamadak-exif/0.6.1)
- License: **BSD-2-Clause**, reproduced verbatim in `LICENSE.kamadak-exif`.

| File | SHA-256 of unchanged upstream bytes |
| --- | --- |
| exif.jpg | cd243e5d55636e7c5b73325d28105acbb044b6c49087bd218a114c588bf814e4 |
| exif.tif | 0fe205445c995ed0291ae80f0748dda24b02931190ff734139e61dd8660e20de |
| exif.png | aa0acbed88fd5162587945726832e8967532f4fc860f0e76d73df557ab768ddb |
| exif.webp | 17dc81ddbc991f1f5e98cb74b61e9661036823ead13fc141e50a6daf993b0177 |
| exif.heic | 6feefa4f4631258be2daad9f92381615417c1acb387a8ca0075968fa2f21a0e5 |

These are small upstream interoperability test containers, **not a camera-photo
corpus**. Tests exercise EXIF extraction and CLI planning; they do not decode or
validate image pixels.

The original JPEG contains `DateTime = 2016:05:04 03:02:01`. The other four
original fixtures contain EXIF fields but no capture/modification date tags, so
hizuke correctly uses their file modification time.

`tests/exif_containers.rs` also creates temporary date-bearing derivatives in
memory. It retains existing container/pixel bytes and existing TIFF field data,
appends replacement IFDs containing three deliberately different EXIF date tags,
and updates container lengths, the PNG CRC, or the HEIC item extent as needed.
Those generated variants verify date precedence and canonical target names for
all five containers. Truncated prefixes and damaged headers are generated only
inside temporary test directories; the checked-in fixture files remain unchanged.
