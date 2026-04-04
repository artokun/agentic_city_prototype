use bevy::prelude::*;
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemType {
    GoldCoin,
    GoldEgg,
}

impl fmt::Display for ItemType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ItemType::GoldCoin => write!(f, "gold_coin"),
            ItemType::GoldEgg => write!(f, "gold_egg"),
        }
    }
}

#[derive(Component, Default, Debug, Clone)]
pub struct Inventory {
    pub items: HashMap<ItemType, u32>,
}

impl Inventory {
    pub fn add(&mut self, item: ItemType, count: u32) {
        *self.items.entry(item).or_insert(0) += count;
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

    pub fn has(&self, item: ItemType, count: u32) -> bool {
        self.items.get(&item).is_some_and(|c| *c >= count)
    }

    pub fn count(&self, item: ItemType) -> u32 {
        self.items.get(&item).copied().unwrap_or(0)
    }
}
