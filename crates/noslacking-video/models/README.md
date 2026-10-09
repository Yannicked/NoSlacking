# Models

`selfie_segmenter_landscape.onnx`: Google's MediaPipe Selfie Segmenter,
landscape (input 256×144), converted from
`https://storage.googleapis.com/mediapipe-models/image_segmenter/selfie_segmenter_landscape/float16/latest/selfie_segmenter_landscape.tflite`
(SHA-256 `490e9ea734313e0de10fa0cd9e3c6133e36ea4db2b7a49bde9ef019f72796b8e`)
by `tools/selfie-onnx/convert.py`. Licensed under the Apache License 2.0 by
Google, as its model card ("Model Card MediaPipe Selfie Segmentation")
states. The background blur (`src/blur.rs`) runs it.
