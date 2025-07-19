use super::*;
use stoat_core::view::GridPosition;

#[test]
fn grid_to_screen_conversion() {
    let layout = GridLayout::new();

    // Test origin
    let origin = GridPosition::new(0, 0);
    let (x, y) = layout.grid_to_screen(origin);
    assert_eq!(x, 0.0);
    assert_eq!(y, 0.0);

    // Test positive coordinates
    let pos = GridPosition::new(1, 2);
    let (x, y) = layout.grid_to_screen(pos);
    assert_eq!(x, 0.0 + 2.0 * (200.0 + 20.0));
    assert_eq!(y, 0.0 + 1.0 * (150.0 + 20.0));

    // Test negative coordinates
    let neg_pos = GridPosition::new(-1, -1);
    let (x, y) = layout.grid_to_screen(neg_pos);
    assert_eq!(x, 0.0 - 1.0 * (200.0 + 20.0));
    assert_eq!(y, 0.0 - 1.0 * (150.0 + 20.0));
}

#[test]
fn screen_to_grid_conversion() {
    let layout = GridLayout::new();

    // Test origin
    let grid = layout.screen_to_grid(0.0, 0.0);
    assert_eq!(grid, GridPosition::new(0, 0));

    // Test cell center
    let grid = layout.screen_to_grid(100.0, 75.0);
    assert_eq!(grid, GridPosition::new(0, 0));

    // Test next cell
    let grid = layout.screen_to_grid(220.0, 170.0);
    assert_eq!(grid, GridPosition::new(1, 1));
}

#[test]
fn hit_test() {
    let layout = GridLayout::new();
    let pos = GridPosition::new(0, 0);

    // Inside cell
    assert!(layout.hit_test(100.0, 75.0, pos));

    // Outside cell
    assert!(!layout.hit_test(500.0, 500.0, pos));

    // Edge cases
    assert!(layout.hit_test(0.0, 0.0, pos)); // Top-left corner
    assert!(layout.hit_test(200.0, 150.0, pos)); // Bottom-right corner
    assert!(!layout.hit_test(201.0, 151.0, pos)); // Just outside
}
