# densezip benchmarks

Machine: 2× Xeon E5-2683 v4 (32 cores / 64 threads), 125 GiB RAM.
Competitors: `gzip -9`, `bzip2 -9`, `xz -9e`, `zstd --ultra -22 --long=27`,
`7z -mx=9` (LZMA2). The **"vs best" column compares densezip against the
best competitor for each individual file** — a stricter baseline than any
single tool. Every densezip archive in these tables was verified
(`dnz t`) to reconstruct all inputs bit-exactly.

Reproduce with `bench/bench.sh <corpus-dir>`.

## Realistic corpus

Mixed real-world formats: two synthetic samples (`sample.pdf` — 80 pages of
"Et Lorem" text with FlateDecode streams; `sample.png` — a 2200×1160
desktop-screenshot-style image), plus a SQLite database, a JPEG photo, a
noisy photographic PNG, a Word document, CSV data, and a gzipped source
tarball.

| file | original | gzip-9 | bzip2-9 | xz-9e | zstd-22 | 7z-mx9 | densezip | vs best |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| inventory.db | 1,409,024 | 342,242 | 212,311 | 157,108 | 195,934 | 158,418 | **74,365** | **−52.7%** |
| photo.jpg | 345,670 | 339,686 | 325,424 | 339,356 | 337,526 | 338,996 | **257,170** | **−21.0%** |
| photo.png | 3,722,509 | 3,723,107 | 3,741,754 | 3,722,756 | 3,722,609 | 3,722,863 | **3,686,156** | **−1.0%** |
| report.docx | 21,679 | 4,951 | 5,732 | 4,400 | 4,647 | 4,485 | **3,226** | **−26.7%** |
| sales.csv | 3,142,709 | 341,330 | 85,526 | 7,656 | 9,966 | 10,206 | **6,008** | **−21.5%** |
| sample.pdf | 91,775 | 73,141 | 73,531 | 72,100 | 72,039 | 72,304 | **31,559** | **−56.2%** |
| sample.png | 236,301 | 208,494 | 212,466 | 202,784 | 203,126 | 202,643 | **42,193** | **−79.2%** |
| src.tar.gz | 31,956 | 31,948 | 32,436 | 32,016 | 31,970 | 32,090 | **21,015** | **−34.2%** |
| **TOTAL** | 9,001,623 | | | | | best-of: 4,523,827 | **4,121,692** | **−8.9%** |

densezip wins on **every file**. Notes:

* `sample.png` (−79%): screenshot-style image — IDAT recompression +
  unfiltering + backend racing crush flat UI regions and rendered text.
* `sample.pdf` (−56%): FlateDecode streams un-deflated, then the text inside
  is compressed by the dzcm context-mixing engine.
* `inventory.db` (−53%): dzcm's record model locks onto SQLite page/row structure.
* `src.tar.gz` / `report.docx` (−27…−34%): preflate deflate recompression —
  competitors see only high-entropy bytes here.
* `photo.jpg` (−21%): lossless lepton JPEG recompression.
* `photo.png` (−1%): worst case — photographic noise is near-incompressible;
  the win comes from unfiltering + sub-green decorrelation. Never a loss,
  because racing includes the identity path.

## Silesia corpus (general-purpose data)

Standard 12-file compression corpus (~212 MB): text, XML, executables,
databases, medical imaging, etc.

| file | original | gzip-9 | bzip2-9 | xz-9e | zstd-22 | 7z-mx9 | densezip | vs best |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| dickens | 10,192,446 | 3,851,823 | 2,799,520 | 2,831,212 | 2,849,381 | 2,831,111 | **2,201,856** | **−21.3%** |
| mozilla | 51,220,480 | 18,994,142 | 17,914,392 | 13,376,240 | 14,967,572 | 13,344,686 | **12,005,293** | **−10.0%** |
| mr | 9,970,564 | 3,673,940 | 2,441,280 | 2,751,892 | 3,105,643 | 2,748,257 | **2,310,959** | **−5.3%** |
| nci | 33,553,445 | 2,987,533 | 1,812,734 | 1,449,272 | 1,610,427 | 1,741,410 | **1,447,354** | **−0.1%** |
| ooffice | 6,152,192 | 3,090,442 | 2,862,526 | 2,427,224 | 2,598,777 | 2,425,568 | **1,846,011** | **−23.9%** |
| osdb | 10,085,684 | 3,716,342 | 2,802,792 | 2,844,556 | 3,098,444 | 2,851,796 | **2,313,439** | **−17.5%** |
| reymont | 6,627,202 | 1,820,834 | 1,246,230 | 1,315,592 | 1,347,556 | 1,318,394 | **999,824** | **−19.8%** |
| samba | 21,606,400 | 5,408,272 | 4,549,759 | 3,739,524 | 3,876,634 | 3,759,770 | **2,991,390** | **−20.0%** |
| sao | 7,251,944 | 5,327,041 | 4,940,524 | 4,425,664 | 5,000,515 | 4,413,926 | **3,847,341** | **−12.8%** |
| webster | 41,458,703 | 12,061,624 | 8,644,714 | 8,368,672 | 8,458,469 | 8,388,839 | **6,067,846** | **−27.5%** |
| xml | 5,345,280 | 662,284 | 441,186 | 434,892 | 453,173 | 455,003 | **362,768** | **−16.6%** |
| x-ray | 8,474,240 | 6,037,713 | 4,051,112 | 4,491,264 | 5,155,752 | 4,479,871 | **3,796,802** | **−6.3%** |
| **TOTAL** | 211,938,580 | | | | | best-of: 47,517,474 | **40,190,883** | **−15.4%** |

densezip wins on **all 12 files**: 15.4% smaller in total than the best
competitor chosen per file. The closest call is `nci` (−0.1%, decided by the
multi-parameter LZMA race); the largest margins are on text (webster −27.5%,
dickens −21.3%) and executables (ooffice −23.9%, samba −20.0%) where the
dzcm context-mixing engine wins outright.

## dzcm engine vs zpaq -m5 (context-mixing reference)

Measured on raw files with the dev command `dnz raw <file> --backend cm`:

| file | xz -9e | zpaq -m5 | dzcm | dzcm vs xz |
|---|---:|---:|---:|---:|
| silesia/xml | 434,892 | 326,956 | 362,684 | −16.6% |
| silesia/dickens | 2,831,212 | 2,094,756 | 2,201,767 | −22.2% |
| silesia/ooffice | 2,427,224 | 1,766,563 | 1,845,923 | −24.0% |
| silesia/sao | 4,425,664 | 3,899,267 | **3,847,256** | −13.1% |

dzcm beats xz everywhere by double digits and already edges out zpaq -m5 on
record-structured data; zpaq -m5 keeps a mid-single-digit lead on pure text.
The archive-level results above don't depend on winning that race: backend
racing takes whichever codec is smallest per stream.
