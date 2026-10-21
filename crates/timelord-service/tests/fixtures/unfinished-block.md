`unfinished-block.bin` is a public mainnet `NewUnfinishedBlockTimelord` payload
captured from the local Chia full node on 2026-10-20. Its signage index is 60,
difficulty 2416, sub-slot iterations 587202560, and appended `header_mmr_root`
is `None`. Python Chia deserialization independently confirmed those fields.

The fixture catches a real interoperability failure: the pinned dg_xch type
stops after `rc_prev`, while current Python Chia appends the optional MMR root.
Our compatibility decoder accepts the legacy envelope or the exact absent-root
extension, rejects malformed/trailing bytes, and rejects an active MMR root
until the linked consensus scheduler supports it. The upstream source is unmodified.
