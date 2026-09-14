# Licenses and Algorithmic Attribution

## 1. Project License

Shadow Desktop is licensed under the **Apache License, Version 2.0** (the "License"). You may obtain a copy of the License in the `LICENSE-APACHE` file at the root of the repository or at:

[http://www.apache.org/licenses/LICENSE-2.0](http://www.apache.org/licenses/LICENSE-2.0)

All four workspace crates (`engine`, `crates/config`, `crates/plugin-abi`, and `crates/plugin-test-echo`) strictly maintain the Apache-2.0 license and publish synchronized versions.

---

## 2. Purity and Intellectual Property Guarantees

- **100% Pure Rust**: No external native C/C++ dependencies or FFI wrappers on the core signal paths.
- **No Proprietary Codecs or Formats**: The engine contains **no** Dolby Atmos, DTS:X, MPEG-H, or other proprietary spatial codecs, bitstream decoders, or encrypted metadata formats.
- **No Patented Spatial Algorithms**: All spatial audio primitives and DSP implementations are derived exclusively from established academic literature, public-domain engineering cookbooks, and open scientific publications.

---

## 3. Academic and Scientific Citations

The audio playback, DSP, and spatial engines rely upon foundational research and open algorithms. Specific citations and attributions include:

### 3.1 Biquad Filters and Parametric Equalization
- **Robert Bristow-Johnson (RBJ)**:
  *Cookbook formulae for audio EQ biquad filter coefficients*, 2005.
  Used in: `src/dsp/biquad.rs`, `src/dsp/channel_trim.rs`, `src/dsp/simd/biquad.rs`.
  Provides minimum-phase low-pass, high-pass, band-pass, peaking, shelving, and all-pass filter topologies with Transposed Direct Form II implementation.

### 3.2 Acoustic Crossovers and Bass Management
- **Siegfried Linkwitz & Russ E. Riley**:
  *Active Crossover Networks for Noncoincident Drivers*, Journal of the Audio Engineering Society (JAES), Vol. 24, No. 1, pp. 2–8, 1976.
  Used in: `src/spatial/bass/filter.rs`, `src/spatial/bass/management.rs`.
  Provides Linkwitz-Riley (LR2, LR4, LR8) crossovers with flat Butterworth-squared magnitude sum, 0° phase difference at crossover frequency, and clean mains-to-subwoofer bass redirection.

### 3.3 Vector Base Amplitude Panning (VBAP)
- **Ville Pulkki**:
  *Virtual Sound Source Positioning Using Vector Base Amplitude Panning*, Journal of the Audio Engineering Society (JAES), Vol. 45, No. 6, pp. 456–466, 1997.
  Used in: `src/spatial/vbap.rs`.
  Provides 3D triangle and 2D pair gain formulation with energy normalization for arbitrary 3D speaker arrays.

### 3.4 Ambisonics & Higher-Order Ambisonics (HOA)
- **Michael A. Gerzon**:
  *Periphony: With-Height Sound Reproduction*, Journal of the Audio Engineering Society (JAES), 1973.
- **Franz Zotter & Matthias Frank**:
  *Ambisonics: A Practical 3D Audio Approach for Sound, Studio, and Info*, Springer Open, 2019.
- **Richard Furse & Dave Malham**:
  *Furse-Malham (FuMa) and ACN/SN3D Higher-Order Ambisonics Specification*.
  Used in: `src/spatial/ambisonic.rs`.
  Provides real spherical harmonic encoding up to 3rd order (16 channels), Wigner D-matrix 3D bus rotation, and energy-preserving max-$r_E$ decoder matrices.

### 3.5 Binaural Head Modeling & HRTF
- **Robert S. Woodworth**:
  *Experimental Psychology*, Holt, New York, 1938.
  Used in: `src/spatial/hrtf.rs`, `src/spatial/binaural.rs`.
  Provides the ray-tracing spherical head Interaural Time Difference (ITD) model: $\text{ITD} = \frac{r}{c}(\theta + \sin\theta)$.
- **Richard O. Duda & William L. Martens**:
  *Range dependence of the response of a spherical head model*, Journal of the Acoustical Society of America (JASA), 104(5), pp. 3048–3058, 1998.
  Used in: `src/spatial/hrtf.rs`.
  Provides single-pole spherical head shadow attenuation and pinna spectral elevation notches.

### 3.6 Room Acoustics and Modal Resonances
- **Manfred R. Schroeder**:
  *Binaural Dissimilarity and Optimum Ceilings for Concert Halls*, Journal of the Acoustical Society of America (JASA), 65(4), 1979.
  Used in: `src/spatial/room.rs`, `src/spatial/acoustic/bass_room.rs`.
  Provides Schroeder cutoff frequency estimation $f_s \approx 2000\sqrt{RT_{60}/V}$, delineating low-frequency wave modal acoustics from high-frequency diffuse reverberation.
- **J. B. Allen & D. A. Berkley**:
  *Image method for efficiently simulating small-room acoustics*, Journal of the Acoustical Society of America (JASA), 65(4), pp. 943–950, 1979.
  Used in: `src/spatial/room.rs`.
  Provides the image-source method for specular early reflections.

### 3.7 Psychoacoustic Bass Immersion & Harmonic Synthesis
- **Missing Fundamental Phenomenon**:
  Auditory pitch perception through harmonic overtones.
  Used in: `src/spatial/bass/engine.rs`.
  Generates 2nd and 3rd harmonics using Chebyshev polynomial non-linearities ($T_2(x) = 2x^2 - 1$, $T_3(x) = 4x^3 - 3x$) with dynamic low-shelf excursion limiting for small speakers and headphones.

### 3.8 Resampling and SIMD Acceleration
- **Julius O. Smith III**:
  *Digital Audio Resampling Home Page*, Center for Computer Research in Music and Acoustics (CCRMA), Stanford University.
  Implemented via Rubato band-limited sinc interpolation (`dsp/resampler/`).
- **SIMD Vectorization**:
  Target-independent fallback kernels and hardware vectorization (SSE2 / AVX2 on x86_64, NEON on aarch64) in `src/dsp/simd/`.
