Generated silent audio, 1.25 seconds, 16 kHz (Vorbis stereo, other formats mono). Tests and deployment require no ffmpeg installation.

Regenerate with `ffmpeg -f lavfi -i anullsrc=r=16000:cl=mono -t 1.25 -c:a CODEC silence.EXT`, using pcm_s16le, libmp3lame, flac, aac, and libopus for WAV, MP3, FLAC, M4A, and WebM. For Ogg use stereo, codec vorbis, and `-strict -2`.
