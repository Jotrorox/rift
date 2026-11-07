These reference zlib streams were generated with Python's standard-library zlib,
independently of Rift's encoder. Regenerate them from the repository root:

```python
from pathlib import Path
import zlib
out = Path('tests/fixtures')
payload = (b'Minecraft compression fixture: ' + bytes(range(256))) * 400
out.joinpath('stored.zlib').write_bytes(zlib.compress(bytes(range(256)), 0))
for name, strategy in [('fixed', zlib.Z_FIXED), ('dynamic', zlib.Z_DEFAULT_STRATEGY)]:
    encoder = zlib.compressobj(9, zlib.DEFLATED, 15, 8, strategy)
    out.joinpath(name + '.zlib').write_bytes(encoder.compress(payload) + encoder.flush())
# Literal frequencies halve per symbol, so Huffman codes reach 15 bits.
ruler = bytes((i & -i).bit_length() - 1 for i in range(1, 16385))
encoder = zlib.compressobj(9, zlib.DEFLATED, 15, 9, zlib.Z_HUFFMAN_ONLY)
out.joinpath('long_codes.zlib').write_bytes(encoder.compress(ruler) + encoder.flush())
```
