Generated silent audio, 1.25 seconds, 16 kHz (Vorbis stereo, other formats mono). The fixtures require no external audio executable.

Regenerate with `ffmpeg -f lavfi -i anullsrc=r=16000:cl=mono -t 1.25 -c:a CODEC silence.EXT`, using pcm_s16le, libmp3lame, flac, aac, and libopus for WAV, MP3, FLAC, M4A, and WebM. For Ogg use stereo, codec vorbis, and `-strict -2`.

`silence-vorbis.webm` is 3 seconds of silent stereo audio at 48 kHz, generated with `ffmpeg -f lavfi -i anullsrc=r=48000:cl=stereo -t 3 -c:a vorbis -strict experimental silence-vorbis.webm`. Its Vorbis track is valid but is not supported by the local WebM duration reader; regression tests use it to verify billing from the provider's `usage.seconds`.
