## Summary

Fixes Enchantment Synergy scope in arena loadout parsing. The xValue multiplier now uses actor-wide repeated property IDs, so stacked FortifyElement properties receive the learned Synergy rank while a single weapon enchant does not.

Players with repeated Fortify Fire/Frost/Shock/Poison item properties and Enchantment Synergy will notice higher matching elemental damage. Single weapon enchant builds should not move from this fix.

## Validation

- Oracle v4 fork-finding Synergy scope test is live and passing.
- Added a spec-loadout unit test for the 99.725 Shock D1 example and the single-weapon-enchant control.
- `SOAK_SCALE=5` soak reports 1200 matches, 0 failed.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
