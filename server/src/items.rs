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
