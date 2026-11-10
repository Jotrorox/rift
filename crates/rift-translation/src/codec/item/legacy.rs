//! 1.12 items (numeric ID and damage) and their 1.13 names, from
//! PrismarineJS minecraft-data's `pc/common/legacy.json` (MIT).
//!
//! INTEGRATION: `crate::legacy` (the data engineer's module) will own this
//! table and its reverse; replace [`name`] and [`legacy`] with calls to it
//! once it exposes them, and delete this file.
use std::{collections::HashMap, sync::OnceLock};

/// `id:damage=name` entries separated by whitespace.
const TABLE: &str = "\
0:0=air 1:0=stone 1:1=granite 1:2=polished_granite 1:3=diorite 1:4=polished_diorite \
1:5=andesite 1:6=polished_andesite 2:0=grass_block 3:0=dirt 3:1=coarse_dirt 3:2=podzol \
4:0=cobblestone 5:0=oak_planks 5:1=spruce_planks 5:2=birch_planks 5:3=jungle_planks \
5:4=acacia_planks 5:5=dark_oak_planks 6:0=oak_sapling 6:1=spruce_sapling \
6:2=birch_sapling 6:3=jungle_sapling 6:4=acacia_sapling 6:5=dark_oak_sapling 7:0=bedrock \
12:0=sand 12:1=red_sand 13:0=gravel 14:0=gold_ore 15:0=iron_ore 16:0=coal_ore \
17:0=oak_log 17:1=spruce_log 17:2=birch_log 17:3=jungle_log 18:0=oak_leaves \
18:1=spruce_leaves 18:2=birch_leaves 18:3=jungle_leaves 19:0=sponge 19:1=wet_sponge \
20:0=glass 21:0=lapis_ore 22:0=lapis_block 23:0=dispenser 24:0=sandstone \
24:1=chiseled_sandstone 24:2=cut_sandstone 25:0=note_block 27:0=powered_rail \
28:0=detector_rail 29:0=sticky_piston 30:0=cobweb 31:1=grass 31:2=fern 32:0=dead_bush \
33:0=piston 35:0=white_wool 35:1=orange_wool 35:2=magenta_wool 35:3=light_blue_wool \
35:4=yellow_wool 35:5=lime_wool 35:6=pink_wool 35:7=gray_wool 35:8=light_gray_wool \
35:9=cyan_wool 35:10=purple_wool 35:11=blue_wool 35:12=brown_wool 35:13=green_wool \
35:14=red_wool 35:15=black_wool 37:0=dandelion 38:0=poppy 38:1=blue_orchid 38:2=allium \
38:3=azure_bluet 38:4=red_tulip 38:5=orange_tulip 38:6=white_tulip 38:7=pink_tulip \
38:8=oxeye_daisy 39:0=brown_mushroom 40:0=red_mushroom 41:0=gold_block 42:0=iron_block \
44:0=stone_slab 44:1=sandstone_slab 44:3=cobblestone_slab 44:4=brick_slab \
44:5=stone_brick_slab 44:6=nether_brick_slab 44:7=quartz_slab 45:0=bricks 46:0=tnt \
47:0=bookshelf 48:0=mossy_cobblestone 49:0=obsidian 50:0=torch 52:0=spawner \
53:0=oak_stairs 54:0=chest 56:0=diamond_ore 57:0=diamond_block 58:0=crafting_table \
60:0=farmland 61:0=furnace 65:0=ladder 66:0=rail 67:0=cobblestone_stairs 69:0=lever \
70:0=stone_pressure_plate 72:0=oak_pressure_plate 73:0=redstone_ore 76:0=redstone_torch \
77:0=stone_button 78:0=snow 79:0=ice 80:0=snow_block 81:0=cactus 82:0=clay 84:0=jukebox \
85:0=oak_fence 86:0=carved_pumpkin 87:0=netherrack 88:0=soul_sand 89:0=glowstone \
91:0=jack_o_lantern 95:0=white_stained_glass 95:1=orange_stained_glass \
95:2=magenta_stained_glass 95:3=light_blue_stained_glass 95:4=yellow_stained_glass \
95:5=lime_stained_glass 95:6=pink_stained_glass 95:7=gray_stained_glass \
95:8=light_gray_stained_glass 95:9=cyan_stained_glass 95:10=purple_stained_glass \
95:11=blue_stained_glass 95:12=brown_stained_glass 95:13=green_stained_glass \
95:14=red_stained_glass 95:15=black_stained_glass 96:0=oak_trapdoor 97:0=infested_stone \
97:1=infested_cobblestone 97:2=infested_stone_bricks 97:3=infested_mossy_stone_bricks \
97:4=infested_cracked_stone_bricks 97:5=infested_chiseled_stone_bricks 98:0=stone_bricks \
98:1=mossy_stone_bricks 98:2=cracked_stone_bricks 98:3=chiseled_stone_bricks \
99:0=brown_mushroom_block 100:0=red_mushroom_block 101:0=iron_bars 102:0=glass_pane \
103:0=melon 106:0=vine 107:0=oak_fence_gate 108:0=brick_stairs 109:0=stone_brick_stairs \
110:0=mycelium 111:0=lily_pad 112:0=nether_bricks 113:0=nether_brick_fence \
114:0=nether_brick_stairs 116:0=enchanting_table 120:0=end_portal_frame 121:0=end_stone \
122:0=dragon_egg 123:0=redstone_lamp 126:0=oak_slab 126:1=spruce_slab 126:2=birch_slab \
126:3=jungle_slab 126:4=acacia_slab 126:5=dark_oak_slab 128:0=sandstone_stairs \
129:0=emerald_ore 130:0=ender_chest 131:0=tripwire_hook 133:0=emerald_block \
134:0=spruce_stairs 135:0=birch_stairs 136:0=jungle_stairs 137:0=command_block \
138:0=beacon 139:0=cobblestone_wall 139:1=mossy_cobblestone_wall 143:0=oak_button \
145:0=anvil 145:1=chipped_anvil 145:2=damaged_anvil 146:0=trapped_chest \
147:0=light_weighted_pressure_plate 148:0=heavy_weighted_pressure_plate \
151:0=daylight_detector 152:0=redstone_block 153:0=nether_quartz_ore 154:0=hopper \
155:0=quartz_block 155:1=chiseled_quartz_block 155:2=quartz_pillar 156:0=quartz_stairs \
157:0=activator_rail 158:0=dropper 159:0=white_terracotta 159:1=orange_terracotta \
159:2=magenta_terracotta 159:3=light_blue_terracotta 159:4=yellow_terracotta \
159:5=lime_terracotta 159:6=pink_terracotta 159:7=gray_terracotta \
159:8=light_gray_terracotta 159:9=cyan_terracotta 159:10=purple_terracotta \
159:11=blue_terracotta 159:12=brown_terracotta 159:13=green_terracotta \
159:14=red_terracotta 159:15=black_terracotta 160:0=white_stained_glass_pane \
160:1=orange_stained_glass_pane 160:2=magenta_stained_glass_pane \
160:3=light_blue_stained_glass_pane 160:4=yellow_stained_glass_pane \
160:5=lime_stained_glass_pane 160:6=pink_stained_glass_pane \
160:7=gray_stained_glass_pane 160:8=light_gray_stained_glass_pane \
160:9=cyan_stained_glass_pane 160:10=purple_stained_glass_pane \
160:11=blue_stained_glass_pane 160:12=brown_stained_glass_pane \
160:13=green_stained_glass_pane 160:14=red_stained_glass_pane \
160:15=black_stained_glass_pane 161:0=acacia_leaves 161:1=dark_oak_leaves \
162:0=acacia_log 162:1=dark_oak_log 163:0=acacia_stairs 164:0=dark_oak_stairs \
165:0=slime_block 166:0=barrier 167:0=iron_trapdoor 168:0=prismarine \
168:1=prismarine_bricks 168:2=dark_prismarine 169:0=sea_lantern 170:0=hay_block \
171:0=white_carpet 171:1=orange_carpet 171:2=magenta_carpet 171:3=light_blue_carpet \
171:4=yellow_carpet 171:5=lime_carpet 171:6=pink_carpet 171:7=gray_carpet \
171:8=light_gray_carpet 171:9=cyan_carpet 171:10=purple_carpet 171:11=blue_carpet \
171:12=brown_carpet 171:13=green_carpet 171:14=red_carpet 171:15=black_carpet \
172:0=terracotta 173:0=coal_block 174:0=packed_ice 175:0=sunflower 175:1=lilac \
175:2=tall_grass 175:3=large_fern 175:4=rose_bush 175:5=peony 179:0=red_sandstone \
179:1=chiseled_red_sandstone 179:2=cut_red_sandstone 180:0=red_sandstone_stairs \
182:0=red_sandstone_slab 183:0=spruce_fence_gate 184:0=birch_fence_gate \
185:0=jungle_fence_gate 186:0=dark_oak_fence_gate 187:0=acacia_fence_gate \
188:0=spruce_fence 189:0=birch_fence 190:0=jungle_fence 191:0=dark_oak_fence \
192:0=acacia_fence 198:0=end_rod 199:0=chorus_plant 200:0=chorus_flower \
201:0=purpur_block 202:0=purpur_pillar 203:0=purpur_stairs 205:0=purpur_slab \
206:0=end_stone_bricks 208:0=grass_path 210:0=repeating_command_block \
211:0=chain_command_block 213:0=magma_block 214:0=nether_wart_block \
215:0=red_nether_bricks 216:0=bone_block 217:0=structure_void 218:0=observer \
219:0=white_shulker_box 220:0=orange_shulker_box 221:0=magenta_shulker_box \
222:0=light_blue_shulker_box 223:0=yellow_shulker_box 224:0=lime_shulker_box \
225:0=pink_shulker_box 226:0=gray_shulker_box 227:0=light_gray_shulker_box \
228:0=cyan_shulker_box 229:0=purple_shulker_box 230:0=blue_shulker_box \
231:0=brown_shulker_box 232:0=green_shulker_box 233:0=red_shulker_box \
234:0=black_shulker_box 235:0=white_glazed_terracotta 236:0=orange_glazed_terracotta \
237:0=magenta_glazed_terracotta 238:0=light_blue_glazed_terracotta \
239:0=yellow_glazed_terracotta 240:0=lime_glazed_terracotta 241:0=pink_glazed_terracotta \
242:0=gray_glazed_terracotta 243:0=light_gray_glazed_terracotta \
244:0=cyan_glazed_terracotta 245:0=purple_glazed_terracotta 246:0=blue_glazed_terracotta \
247:0=brown_glazed_terracotta 248:0=green_glazed_terracotta 249:0=red_glazed_terracotta \
250:0=black_glazed_terracotta 251:0=white_concrete 251:1=orange_concrete \
251:2=magenta_concrete 251:3=light_blue_concrete 251:4=yellow_concrete \
251:5=lime_concrete 251:6=pink_concrete 251:7=gray_concrete 251:8=light_gray_concrete \
251:9=cyan_concrete 251:10=purple_concrete 251:11=blue_concrete 251:12=brown_concrete \
251:13=green_concrete 251:14=red_concrete 251:15=black_concrete \
252:0=white_concrete_powder 252:1=orange_concrete_powder 252:2=magenta_concrete_powder \
252:3=light_blue_concrete_powder 252:4=yellow_concrete_powder 252:5=lime_concrete_powder \
252:6=pink_concrete_powder 252:7=gray_concrete_powder 252:8=light_gray_concrete_powder \
252:9=cyan_concrete_powder 252:10=purple_concrete_powder 252:11=blue_concrete_powder \
252:12=brown_concrete_powder 252:13=green_concrete_powder 252:14=red_concrete_powder \
252:15=black_concrete_powder 255:0=structure_block 256:0=iron_shovel 257:0=iron_pickaxe \
258:0=iron_axe 259:0=flint_and_steel 260:0=apple 261:0=bow 262:0=arrow 263:0=coal \
263:1=charcoal 264:0=diamond 265:0=iron_ingot 266:0=gold_ingot 267:0=iron_sword \
268:0=wooden_sword 269:0=wooden_shovel 270:0=wooden_pickaxe 271:0=wooden_axe \
272:0=stone_sword 273:0=stone_shovel 274:0=stone_pickaxe 275:0=stone_axe \
276:0=diamond_sword 277:0=diamond_shovel 278:0=diamond_pickaxe 279:0=diamond_axe \
280:0=stick 281:0=bowl 282:0=mushroom_stew 283:0=golden_sword 284:0=golden_shovel \
285:0=golden_pickaxe 286:0=golden_axe 287:0=string 288:0=feather 289:0=gunpowder \
290:0=wooden_hoe 291:0=stone_hoe 292:0=iron_hoe 293:0=diamond_hoe 294:0=golden_hoe \
295:0=wheat_seeds 296:0=wheat 297:0=bread 298:0=leather_helmet 299:0=leather_chestplate \
300:0=leather_leggings 301:0=leather_boots 302:0=chainmail_helmet \
303:0=chainmail_chestplate 304:0=chainmail_leggings 305:0=chainmail_boots \
306:0=iron_helmet 307:0=iron_chestplate 308:0=iron_leggings 309:0=iron_boots \
310:0=diamond_helmet 311:0=diamond_chestplate 312:0=diamond_leggings 313:0=diamond_boots \
314:0=golden_helmet 315:0=golden_chestplate 316:0=golden_leggings 317:0=golden_boots \
318:0=flint 319:0=porkchop 320:0=cooked_porkchop 321:0=painting 322:0=golden_apple \
322:1=enchanted_golden_apple 323:0=sign 324:0=oak_door 325:0=bucket 326:0=water_bucket \
327:0=lava_bucket 328:0=minecart 329:0=saddle 330:0=iron_door 331:0=redstone \
332:0=snowball 333:0=oak_boat 334:0=leather 335:0=milk_bucket 336:0=brick \
337:0=clay_ball 338:0=sugar_cane 339:0=paper 340:0=book 341:0=slime_ball \
342:0=chest_minecart 343:0=furnace_minecart 344:0=egg 345:0=compass 346:0=fishing_rod \
347:0=clock 348:0=glowstone_dust 349:0=cod 349:1=salmon 349:2=tropical_fish \
349:3=pufferfish 350:0=cooked_cod 350:1=cooked_salmon 351:0=ink_sac 351:1=rose_red \
351:2=cactus_green 351:3=cocoa_beans 351:4=lapis_lazuli 351:5=purple_dye 351:6=cyan_dye \
351:7=light_gray_dye 351:8=gray_dye 351:9=pink_dye 351:10=lime_dye \
351:11=dandelion_yellow 351:12=light_blue_dye 351:13=magenta_dye 351:14=orange_dye \
351:15=bone_meal 352:0=bone 353:0=sugar 354:0=cake 355:0=white_bed 355:1=orange_bed \
355:2=magenta_bed 355:3=light_blue_bed 355:4=yellow_bed 355:5=lime_bed 355:6=pink_bed \
355:7=gray_bed 355:8=light_gray_bed 355:9=cyan_bed 355:10=purple_bed 355:11=blue_bed \
355:12=brown_bed 355:13=green_bed 355:14=red_bed 355:15=black_bed 356:0=repeater \
357:0=cookie 358:0=filled_map 359:0=shears 360:0=melon_slice 361:0=pumpkin_seeds \
362:0=melon_seeds 363:0=beef 364:0=cooked_beef 365:0=chicken 366:0=cooked_chicken \
367:0=rotten_flesh 368:0=ender_pearl 369:0=blaze_rod 370:0=ghast_tear 371:0=gold_nugget \
372:0=nether_wart 373:0=potion 374:0=glass_bottle 375:0=spider_eye \
376:0=fermented_spider_eye 377:0=blaze_powder 378:0=magma_cream 379:0=brewing_stand \
380:0=cauldron 381:0=ender_eye 382:0=glistering_melon_slice 383:0=pig_spawn_egg \
384:0=experience_bottle 385:0=fire_charge 386:0=writable_book 387:0=written_book \
388:0=emerald 389:0=item_frame 390:0=flower_pot 391:0=carrot 392:0=potato \
393:0=baked_potato 394:0=poisonous_potato 395:0=map 396:0=golden_carrot \
397:0=skeleton_skull 397:1=wither_skeleton_skull 397:2=zombie_head 397:3=player_head \
397:4=creeper_head 397:5=dragon_head 398:0=carrot_on_a_stick 399:0=nether_star \
400:0=pumpkin_pie 401:0=firework_rocket 402:0=firework_star 403:0=enchanted_book \
404:0=comparator 405:0=nether_brick 406:0=quartz 407:0=tnt_minecart \
408:0=hopper_minecart 409:0=prismarine_shard 410:0=prismarine_crystals 411:0=rabbit \
412:0=cooked_rabbit 413:0=rabbit_stew 414:0=rabbit_foot 415:0=rabbit_hide \
416:0=armor_stand 417:0=iron_horse_armor 418:0=golden_horse_armor \
419:0=diamond_horse_armor 420:0=lead 421:0=name_tag 422:0=command_block_minecart \
423:0=mutton 424:0=cooked_mutton 425:0=black_banner 425:1=red_banner 425:2=green_banner \
425:3=brown_banner 425:4=blue_banner 425:5=purple_banner 425:6=cyan_banner \
425:7=light_gray_banner 425:8=gray_banner 425:9=pink_banner 425:10=lime_banner \
425:11=yellow_banner 425:12=light_blue_banner 425:13=magenta_banner 425:14=orange_banner \
425:15=white_banner 426:0=end_crystal 427:0=spruce_door 428:0=birch_door \
429:0=jungle_door 430:0=acacia_door 431:0=dark_oak_door 432:0=chorus_fruit \
433:0=popped_chorus_fruit 434:0=beetroot 435:0=beetroot_seeds 436:0=beetroot_soup \
437:0=dragon_breath 438:0=splash_potion 439:0=spectral_arrow 440:0=tipped_arrow \
441:0=lingering_potion 442:0=shield 443:0=elytra 444:0=spruce_boat 445:0=birch_boat \
446:0=jungle_boat 447:0=acacia_boat 448:0=dark_oak_boat 449:0=totem_of_undying \
450:0=shulker_shell 452:0=iron_nugget 453:0=knowledge_book 2256:0=music_disc_13 \
2257:0=music_disc_cat 2258:0=music_disc_blocks 2259:0=music_disc_chirp \
2260:0=music_disc_far 2261:0=music_disc_mall 2262:0=music_disc_mellohi \
2263:0=music_disc_stal 2264:0=music_disc_strad 2265:0=music_disc_ward \
2266:0=music_disc_11 2267:0=music_disc_wait";

struct Tables {
    names: HashMap<(i16, i16), &'static str>,
    legacy: HashMap<&'static str, (i16, i16)>,
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut names = HashMap::new();
        let mut legacy = HashMap::new();
        for entry in TABLE.split_whitespace() {
            let (key, name) = entry.split_once('=').expect("legacy item entry");
            let (id, damage) = key.split_once(':').expect("legacy item key");
            let key = (
                id.parse().expect("legacy item ID"),
                damage.parse().expect("legacy item damage"),
            );
            names.insert(key, name);
            // The first (lowest) legacy form wins for the reverse.
            legacy.entry(name).or_insert(key);
        }
        Tables { names, legacy }
    })
}

/// The 1.13 name of the 1.12 item `id` with data value `damage`.
pub fn name(id: i16, damage: i16) -> Option<&'static str> {
    tables().names.get(&(id, damage)).copied()
}

/// The 1.12 item and data value for a 1.13 item name, when 1.12 had it.
pub fn legacy(name: &str) -> Option<(i16, i16)> {
    tables().legacy.get(name).copied()
}
