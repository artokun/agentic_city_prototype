use bevy::prelude::*;
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemType {
    GoldCoin,
    GoldEgg,
    Coffee,
    Muffin,
    Rations,
    Sandwich,
    Soup,
    Paycheck,
    Document,
    // Raw materials
    CoffeeBeans,
    Flour,
    RawMeat,
}

impl ItemType {
    /// Wholesale price at the warehouse (gold per unit).
    pub fn wholesale_price(&self) -> Option<(u32, u32)> {
        match self {
            ItemType::CoffeeBeans => Some((1, 20)), // 1g per 20 beans
            ItemType::Flour => Some((1, 15)),       // 1g per 15 flour
            ItemType::RawMeat => Some((1, 10)),     // 1g per 10 raw meat
            _ => None,
        }
    }

    /// If this is a raw material, returns (output ItemType, ticks to process one unit).
    pub fn processing_recipe(&self) -> Option<(ItemType, u32)> {
        match self {
            ItemType::CoffeeBeans => Some((ItemType::Coffee, 10)),
            ItemType::Flour => Some((ItemType::Muffin, 15)),
            ItemType::RawMeat => Some((ItemType::Sandwich, 12)),
            _ => None,
        }
    }

    pub fn is_raw_material(&self) -> bool {
        self.processing_recipe().is_some()
    }

    /// If this is a finished good that can be produced from a raw material,
    /// returns the raw material needed.
    pub fn raw_ingredient(&self) -> Option<ItemType> {
        match self {
            ItemType::Coffee => Some(ItemType::CoffeeBeans),
            ItemType::Muffin => Some(ItemType::Flour),
            ItemType::Sandwich => Some(ItemType::RawMeat),
            _ => None,
        }
    }

    /// Retail price when a customer buys one.
    pub fn retail_price(&self) -> u32 {
        match self {
            ItemType::Coffee => 1,
            ItemType::Muffin => 1,
            ItemType::Sandwich => 1,
            ItemType::Soup => 1,
            ItemType::Rations => 0, // free at apartments
            _ => 0,
        }
    }

    pub fn is_food(&self) -> bool {
        matches!(self, ItemType::Coffee | ItemType::Muffin | ItemType::Rations | ItemType::Sandwich | ItemType::Soup)
    }
}

impl fmt::Display for ItemType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ItemType::GoldCoin => write!(f, "gold_coin"),
            ItemType::GoldEgg => write!(f, "gold_egg"),
            ItemType::Coffee => write!(f, "coffee"),
            ItemType::Muffin => write!(f, "muffin"),
            ItemType::Rations => write!(f, "rations"),
            ItemType::Sandwich => write!(f, "sandwich"),
            ItemType::Soup => write!(f, "soup"),
            ItemType::Paycheck => write!(f, "paycheck"),
            ItemType::Document => write!(f, "document"),
            ItemType::CoffeeBeans => write!(f, "coffee_beans"),
            ItemType::Flour => write!(f, "flour"),
            ItemType::RawMeat => write!(f, "raw_meat"),
        }
    }
}

#[derive(Component, Default, Debug, Clone)]
pub struct Inventory {
    pub items: HashMap<ItemType, u32>,
    /// Gold debt — only GoldCoin can go negative via bounty recycling.
    pub gold_debt: u32,
}

impl Inventory {
    pub fn add(&mut self, item: ItemType, count: u32) {
        if item == ItemType::GoldCoin && self.gold_debt > 0 {
            // Pay off debt first.
            if count >= self.gold_debt {
                let remainder = count - self.gold_debt;
                self.gold_debt = 0;
                if remainder > 0 {
                    *self.items.entry(item).or_insert(0) += remainder;
                }
            } else {
                self.gold_debt -= count;
            }
            return;
        }
        *self.items.entry(item).or_insert(0) += count;
    }

    pub fn add_capped(&mut self, item: ItemType, count: u32, max: u32) -> u32 {
        let current = self.count(item);
        let can_add = (max - current).min(count);
        if can_add > 0 {
            self.add(item, can_add);
        }
        can_add
    }

    pub fn remove(&mut self, item: ItemType, count: u32) -> bool {
        if let Some(current) = self.items.get_mut(&item) {
            if *current >= count {
                *current -= count;
                if *current == 0 {
                    self.items.remove(&item);
                }
                return true;
            }
        }
        false
    }

    /// Deduct gold, allowing debt if the agent doesn't have enough.
    /// Returns the actual amount deducted from on-hand gold (may be less than cost).
    pub fn deduct_gold_with_debt(&mut self, cost: u32) {
        let on_hand = self.count(ItemType::GoldCoin);
        if on_hand >= cost {
            self.remove(ItemType::GoldCoin, cost);
        } else {
            // Remove what we have, put the rest into debt.
            if on_hand > 0 {
                self.items.remove(&ItemType::GoldCoin);
            }
            self.gold_debt += cost - on_hand;
        }
    }

    /// Net gold balance (negative if in debt).
    pub fn gold_balance(&self) -> i32 {
        self.count(ItemType::GoldCoin) as i32 - self.gold_debt as i32
    }

    pub fn has(&self, item: ItemType, count: u32) -> bool {
        self.items.get(&item).is_some_and(|c| *c >= count)
    }

    pub fn count(&self, item: ItemType) -> u32 {
        self.items.get(&item).copied().unwrap_or(0)
    }
}

/// Holds documents an agent has produced (title → content).
#[derive(Component, Default, Debug, Clone)]
pub struct DocumentInventory {
    pub documents: HashMap<String, String>,
}

impl DocumentInventory {
    pub fn add(&mut self, title: String, content: String) {
        self.documents.insert(title, content);
    }

    pub fn has(&self, title: &str) -> bool {
        self.documents.contains_key(title)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Inventory basics ---

    #[test]
    fn add_and_count() {
        let mut inv = Inventory::default();
        inv.add(ItemType::Coffee, 5);
        assert_eq!(inv.count(ItemType::Coffee), 5);
        inv.add(ItemType::Coffee, 3);
        assert_eq!(inv.count(ItemType::Coffee), 8);
    }

    #[test]
    fn remove_success() {
        let mut inv = Inventory::default();
        inv.add(ItemType::Muffin, 10);
        assert!(inv.remove(ItemType::Muffin, 4));
        assert_eq!(inv.count(ItemType::Muffin), 6);
    }

    #[test]
    fn remove_exact_amount_cleans_up() {
        let mut inv = Inventory::default();
        inv.add(ItemType::Muffin, 3);
        assert!(inv.remove(ItemType::Muffin, 3));
        assert_eq!(inv.count(ItemType::Muffin), 0);
        assert!(!inv.items.contains_key(&ItemType::Muffin));
    }

    #[test]
    fn remove_insufficient_returns_false() {
        let mut inv = Inventory::default();
        inv.add(ItemType::Coffee, 2);
        assert!(!inv.remove(ItemType::Coffee, 5));
        // Balance unchanged
        assert_eq!(inv.count(ItemType::Coffee), 2);
    }

    #[test]
    fn remove_missing_item_returns_false() {
        let mut inv = Inventory::default();
        assert!(!inv.remove(ItemType::Soup, 1));
    }

    #[test]
    fn has_checks_minimum() {
        let mut inv = Inventory::default();
        inv.add(ItemType::Sandwich, 3);
        assert!(inv.has(ItemType::Sandwich, 1));
        assert!(inv.has(ItemType::Sandwich, 3));
        assert!(!inv.has(ItemType::Sandwich, 4));
    }

    #[test]
    fn has_missing_item() {
        let inv = Inventory::default();
        assert!(!inv.has(ItemType::Rations, 1));
    }

    // --- Gold debt mechanics ---

    #[test]
    fn deduct_gold_with_enough_on_hand() {
        let mut inv = Inventory::default();
        inv.add(ItemType::GoldCoin, 10);
        inv.deduct_gold_with_debt(4);
        assert_eq!(inv.count(ItemType::GoldCoin), 6);
        assert_eq!(inv.gold_debt, 0);
    }

    #[test]
    fn deduct_gold_creates_debt() {
        let mut inv = Inventory::default();
        inv.add(ItemType::GoldCoin, 3);
        inv.deduct_gold_with_debt(8);
        assert_eq!(inv.count(ItemType::GoldCoin), 0);
        assert_eq!(inv.gold_debt, 5);
    }

    #[test]
    fn deduct_gold_from_empty() {
        let mut inv = Inventory::default();
        inv.deduct_gold_with_debt(5);
        assert_eq!(inv.gold_debt, 5);
        assert_eq!(inv.count(ItemType::GoldCoin), 0);
    }

    #[test]
    fn gold_balance_positive() {
        let mut inv = Inventory::default();
        inv.add(ItemType::GoldCoin, 10);
        assert_eq!(inv.gold_balance(), 10);
    }

    #[test]
    fn gold_balance_negative_with_debt() {
        let mut inv = Inventory::default();
        inv.deduct_gold_with_debt(7);
        assert_eq!(inv.gold_balance(), -7);
    }

    #[test]
    fn add_gold_pays_off_debt_fully() {
        let mut inv = Inventory::default();
        inv.gold_debt = 5;
        inv.add(ItemType::GoldCoin, 8);
        assert_eq!(inv.gold_debt, 0);
        assert_eq!(inv.count(ItemType::GoldCoin), 3);
    }

    #[test]
    fn add_gold_pays_off_debt_partially() {
        let mut inv = Inventory::default();
        inv.gold_debt = 10;
        inv.add(ItemType::GoldCoin, 3);
        assert_eq!(inv.gold_debt, 7);
        assert_eq!(inv.count(ItemType::GoldCoin), 0);
    }

    #[test]
    fn add_gold_pays_off_debt_exactly() {
        let mut inv = Inventory::default();
        inv.gold_debt = 5;
        inv.add(ItemType::GoldCoin, 5);
        assert_eq!(inv.gold_debt, 0);
        assert_eq!(inv.count(ItemType::GoldCoin), 0);
    }

    #[test]
    fn add_non_gold_ignores_debt() {
        let mut inv = Inventory::default();
        inv.gold_debt = 5;
        inv.add(ItemType::Coffee, 3);
        assert_eq!(inv.gold_debt, 5);
        assert_eq!(inv.count(ItemType::Coffee), 3);
    }

    // --- add_capped ---

    #[test]
    fn add_capped_below_max() {
        let mut inv = Inventory::default();
        let added = inv.add_capped(ItemType::Coffee, 5, 10);
        assert_eq!(added, 5);
        assert_eq!(inv.count(ItemType::Coffee), 5);
    }

    #[test]
    fn add_capped_at_max() {
        let mut inv = Inventory::default();
        inv.add(ItemType::Coffee, 8);
        let added = inv.add_capped(ItemType::Coffee, 5, 10);
        assert_eq!(added, 2);
        assert_eq!(inv.count(ItemType::Coffee), 10);
    }

    #[test]
    fn add_capped_already_full() {
        let mut inv = Inventory::default();
        inv.add(ItemType::Muffin, 10);
        let added = inv.add_capped(ItemType::Muffin, 5, 10);
        assert_eq!(added, 0);
        assert_eq!(inv.count(ItemType::Muffin), 10);
    }

    // --- Wholesale prices ---

    #[test]
    fn raw_materials_have_wholesale_prices() {
        assert!(ItemType::CoffeeBeans.wholesale_price().is_some());
        assert!(ItemType::Flour.wholesale_price().is_some());
        assert!(ItemType::RawMeat.wholesale_price().is_some());
    }

    #[test]
    fn finished_goods_have_no_wholesale_price() {
        assert!(ItemType::Coffee.wholesale_price().is_none());
        assert!(ItemType::GoldCoin.wholesale_price().is_none());
    }

    // --- Processing recipes ---

    #[test]
    fn coffee_beans_produce_coffee() {
        let (output, ticks) = ItemType::CoffeeBeans.processing_recipe().unwrap();
        assert_eq!(output, ItemType::Coffee);
        assert_eq!(ticks, 10);
    }

    #[test]
    fn flour_produces_muffin() {
        let (output, ticks) = ItemType::Flour.processing_recipe().unwrap();
        assert_eq!(output, ItemType::Muffin);
        assert_eq!(ticks, 15);
    }

    #[test]
    fn raw_meat_produces_sandwich() {
        let (output, ticks) = ItemType::RawMeat.processing_recipe().unwrap();
        assert_eq!(output, ItemType::Sandwich);
        assert_eq!(ticks, 12);
    }

    #[test]
    fn non_raw_has_no_recipe() {
        assert!(ItemType::Coffee.processing_recipe().is_none());
        assert!(ItemType::GoldCoin.processing_recipe().is_none());
    }

    #[test]
    fn is_raw_material() {
        assert!(ItemType::CoffeeBeans.is_raw_material());
        assert!(ItemType::Flour.is_raw_material());
        assert!(ItemType::RawMeat.is_raw_material());
        assert!(!ItemType::Coffee.is_raw_material());
        assert!(!ItemType::GoldCoin.is_raw_material());
    }

    #[test]
    fn raw_ingredient_roundtrip() {
        assert_eq!(ItemType::Coffee.raw_ingredient(), Some(ItemType::CoffeeBeans));
        assert_eq!(ItemType::Muffin.raw_ingredient(), Some(ItemType::Flour));
        assert_eq!(ItemType::Sandwich.raw_ingredient(), Some(ItemType::RawMeat));
        assert_eq!(ItemType::Soup.raw_ingredient(), None);
    }
}
