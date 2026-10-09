# Models

`selfie_segmenter_landscape.nsseg`: Google's MediaPipe Selfie Segmenter,
landscape (input 256×144), exported from
`https://storage.googleapis.com/mediapipe-models/image_segmenter/selfie_segmenter_landscape/float16/latest/selfie_segmenter_landscape.tflite`
(SHA-256 `490e9ea734313e0de10fa0cd9e3c6133e36ea4db2b7a49bde9ef019f72796b8e`)
by `tools/selfie-onnx/export.py` into the format `src/segment.rs` runs.
Licensed under the Apache License 2.0 by Google, as its model card ("Model
Card MediaPipe Selfie Segmentation") states. The background blur
(`src/blur.rs`) runs it. `tools/selfie-onnx/convert.py` makes an ONNX copy
instead, to check against other runtimes.

`fixtures/portrait_256x144.rgb` is NASA's official portrait of astronaut
Andrew R. Morgan (public domain, a work of NASA), squeezed to 256×144 RGB;
`fixtures/portrait_256x144.mask` is MediaPipe's own mask for it, in bytes
(0 to 255). The segmentation test checks our code against it.
