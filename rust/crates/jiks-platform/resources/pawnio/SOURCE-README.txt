PawnIO module source inputs

The source/ directory contains unmodified IntelMSR.p, SmbusI801.p and
SmbusIntelSkylakeIMC.p, all include files and the upstream build workflow,
README and LGPL-2.1 license from PawnIO.Modules 0.2.11, commit:
52a7e536dff3e53c96917a28caac5e0fa6510696
https://github.com/namazso/PawnIO.Modules/tree/52a7e536dff3e53c96917a28caac5e0fa6510696

source/source-manifest.json records each original Git blob ID, SHA-256 and
immutable source URL. The separately signed .bin files remain unchanged; their
origin and hashes are recorded in NOTICE.txt. No signing key is included.

Upstream uses Pawn compiler 4.1.7152 and these arguments in its build workflow:
  pawncc IntelMSR.p '-iinclude' '-C64' '-;+' '-(+' '-p'
  pawncc SmbusI801.p '-iinclude' '-C64' '-;+' '-(+' '-p'
  pawncc SmbusIntelSkylakeIMC.p '-iinclude' '-C64' '-;+' '-(+' '-p'
Run them from source/ using a separately obtained compatible compiler. The
upstream repository's _pawn directory retains its pinned compiler and source
RPM references. The generic compiler is not embedded or executed by Jiks.

Compiling produces unsigned .amx input, not a claim of reproducing the signed
distribution bytes. Jiks does not sign, install, start or stop a driver. The
adapter embeds the three original signed .bin modules and exposes only its
explicitly enabled, restricted sensor routes. The full application source lets
a developer change those compile-time resources and rebuild the application;
the installed driver's own trust policy still applies.

Public binary packages include these module source inputs under
licenses/PawnIO-source/, alongside the LGPL license and module provenance.
