-- ============================================================
-- VLO FRAMEWORK CRUD TESTING SCHEMA & SEED DUMP
-- Database Target: SQLite (vlo_app)
-- ============================================================

CREATE DATABASE IF NOT EXISTS vlo_app;
USE vlo_apps;

PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;

-- ============================================================
-- 1. USERS TABLE
-- ============================================================

CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    email TEXT UNIQUE NOT NULL,
    role TEXT DEFAULT 'User',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

INSERT INTO users (id, name, email, role)
SELECT 1, 'Mamtaz H.', 'mamtaz@example.com', 'Admin'
WHERE NOT EXISTS (SELECT 1 FROM users WHERE id = 1);

INSERT INTO users (id, name, email, role)
SELECT 2, 'Sarah Connor', 'sarah@example.com', 'Editor'
WHERE NOT EXISTS (SELECT 1 FROM users WHERE id = 2);

INSERT INTO users (id, name, email, role)
SELECT 3, 'Alex Mercer', 'alex@example.com', 'User'
WHERE NOT EXISTS (SELECT 1 FROM users WHERE id = 3);

INSERT INTO users (id, name, email, role)
SELECT 4, 'John Doe', 'john@example.com', 'User'
WHERE NOT EXISTS (SELECT 1 FROM users WHERE id = 4);

INSERT INTO users (id, name, email, role)
SELECT 5, 'Emily Stone', 'emily@example.com', 'Editor'
WHERE NOT EXISTS (SELECT 1 FROM users WHERE id = 5);

INSERT INTO users (id, name, email, role)
SELECT 6, 'David Kim', 'david@example.com', 'User'
WHERE NOT EXISTS (SELECT 1 FROM users WHERE id = 6);

INSERT INTO users (id, name, email, role)
SELECT 7, 'Lisa Wong', 'lisa@example.com', 'User'
WHERE NOT EXISTS (SELECT 1 FROM users WHERE id = 7);

INSERT INTO users (id, name, email, role)
SELECT 8, 'Michael Scott', 'michael@example.com', 'Editor'
WHERE NOT EXISTS (SELECT 1 FROM users WHERE id = 8);

INSERT INTO users (id, name, email, role)
SELECT 9, 'Rachel Green', 'rachel@example.com', 'User'
WHERE NOT EXISTS (SELECT 1 FROM users WHERE id = 9);

INSERT INTO users (id, name, email, role)
SELECT 10, 'Bruce Wayne', 'bruce@example.com', 'Admin'
WHERE NOT EXISTS (SELECT 1 FROM users WHERE id = 10);


-- ============================================================
-- 2. PRODUCTS TABLE
-- ============================================================

CREATE TABLE IF NOT EXISTS products (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    description TEXT,
    price REAL NOT NULL,
    stock INTEGER DEFAULT 0,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

INSERT INTO products (id, title, description, price, stock)
SELECT 1,
       'Mechanical Keyboard',
       'RGB Hot-swappable tactile switch keyboard',
       89.99,
       15
WHERE NOT EXISTS (SELECT 1 FROM products WHERE id = 1);

INSERT INTO products (id, title, description, price, stock)
SELECT 2,
       'Ergonomic Mouse',
       '2.4GHz Wireless dual-mode mouse',
       49.50,
       40
WHERE NOT EXISTS (SELECT 1 FROM products WHERE id = 2);

INSERT INTO products (id, title, description, price, stock)
SELECT 3,
       'Curved Monitor 34"',
       '144Hz WQHD UltraWide display',
       399.00,
       8
WHERE NOT EXISTS (SELECT 1 FROM products WHERE id = 3);

INSERT INTO products (id, title, description, price, stock)
SELECT 4,
       'USB-C Hub',
       '7-in-1 USB-C hub with HDMI and card reader',
       39.99,
       25
WHERE NOT EXISTS (SELECT 1 FROM products WHERE id = 4);

INSERT INTO products (id, title, description, price, stock)
SELECT 5,
       'Laptop Stand',
       'Adjustable aluminum laptop stand',
       59.95,
       18
WHERE NOT EXISTS (SELECT 1 FROM products WHERE id = 5);

INSERT INTO products (id, title, description, price, stock)
SELECT 6,
       'Webcam HD',
       '1080p USB webcam with built-in microphone',
       69.00,
       30
WHERE NOT EXISTS (SELECT 1 FROM products WHERE id = 6);

INSERT INTO products (id, title, description, price, stock)
SELECT 7,
       'Desk Mat',
       'Large waterproof extended desk mat',
       24.99,
       50
WHERE NOT EXISTS (SELECT 1 FROM products WHERE id = 7);

INSERT INTO products (id, title, description, price, stock)
SELECT 8,
       'Bluetooth Speaker',
       'Portable wireless Bluetooth speaker',
       79.50,
       12
WHERE NOT EXISTS (SELECT 1 FROM products WHERE id = 8);

INSERT INTO products (id, title, description, price, stock)
SELECT 9,
       'Noise Cancelling Headphones',
       'Over-ear wireless ANC headphones',
       149.99,
       10
WHERE NOT EXISTS (SELECT 1 FROM products WHERE id = 9);

INSERT INTO products (id, title, description, price, stock)
SELECT 10,
       'Portable SSD 1TB',
       'USB-C external solid state drive',
       109.99,
       20
WHERE NOT EXISTS (SELECT 1 FROM products WHERE id = 10);


-- ============================================================
-- 3. TASKS TABLE
-- ============================================================

CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL,
    title TEXT NOT NULL,
    status TEXT CHECK (
        status IN ('pending', 'in_progress', 'completed')
    ) DEFAULT 'pending',
    is_archived INTEGER DEFAULT 0,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,

    FOREIGN KEY (user_id)
        REFERENCES users(id)
        ON DELETE CASCADE
);

INSERT INTO tasks (id, user_id, title, status, is_archived)
SELECT 1,
       1,
       'Deploy Axum server to Railway',
       'in_progress',
       0
WHERE NOT EXISTS (SELECT 1 FROM tasks WHERE id = 1);

INSERT INTO tasks (id, user_id, title, status, is_archived)
SELECT 2,
       1,
       'Configure SQLite connection pool',
       'completed',
       0
WHERE NOT EXISTS (SELECT 1 FROM tasks WHERE id = 2);

INSERT INTO tasks (id, user_id, title, status, is_archived)
SELECT 3,
       2,
       'Write documentation for VLO framework',
       'pending',
       0
WHERE NOT EXISTS (SELECT 1 FROM tasks WHERE id = 3);

INSERT INTO tasks (id, user_id, title, status, is_archived)
SELECT 4,
       3,
       'Review CRUD endpoints',
       'in_progress',
       0
WHERE NOT EXISTS (SELECT 1 FROM tasks WHERE id = 4);

INSERT INTO tasks (id, user_id, title, status, is_archived)
SELECT 5,
       4,
       'Add validation tests',
       'pending',
       0
WHERE NOT EXISTS (SELECT 1 FROM tasks WHERE id = 5);

INSERT INTO tasks (id, user_id, title, status, is_archived)
SELECT 6,
       5,
       'Update API documentation',
       'completed',
       0
WHERE NOT EXISTS (SELECT 1 FROM tasks WHERE id = 6);

INSERT INTO tasks (id, user_id, title, status, is_archived)
SELECT 7,
       6,
       'Test pagination behavior',
       'in_progress',
       0
WHERE NOT EXISTS (SELECT 1 FROM tasks WHERE id = 7);

INSERT INTO tasks (id, user_id, title, status, is_archived)
SELECT 8,
       7,
       'Implement search filters',
       'pending',
       0
WHERE NOT EXISTS (SELECT 1 FROM tasks WHERE id = 8);

INSERT INTO tasks (id, user_id, title, status, is_archived)
SELECT 9,
       8,
       'Review database indexes',
       'completed',
       0
WHERE NOT EXISTS (SELECT 1 FROM tasks WHERE id = 9);

INSERT INTO tasks (id, user_id, title, status, is_archived)
SELECT 10,
       9,
       'Run integration tests',
       'in_progress',
       0
WHERE NOT EXISTS (SELECT 1 FROM tasks WHERE id = 10);

INSERT INTO tasks (id, user_id, title, status, is_archived)
SELECT 11,
       10,
       'Audit admin permissions',
       'pending',
       0
WHERE NOT EXISTS (SELECT 1 FROM tasks WHERE id = 11);

INSERT INTO tasks (id, user_id, title, status, is_archived)
SELECT 12,
       1,
       'Clean up old records',
       'completed',
       1
WHERE NOT EXISTS (SELECT 1 FROM tasks WHERE id = 12);


-- ============================================================
-- 4. LOGS TABLE
-- ============================================================

CREATE TABLE IF NOT EXISTS logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    action TEXT NOT NULL,
    details TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

INSERT INTO logs (id, action, details)
SELECT 1,
       'INITIALIZE_DB',
       'Schema migrations applied successfully'
WHERE NOT EXISTS (SELECT 1 FROM logs WHERE id = 1);

INSERT INTO logs (id, action, details)
SELECT 2,
       'USER_LOGIN',
       'User #1 logged in'
WHERE NOT EXISTS (SELECT 1 FROM logs WHERE id = 2);

INSERT INTO logs (id, action, details)
SELECT 3,
       'CREATE_USER',
       'User #4 created successfully'
WHERE NOT EXISTS (SELECT 1 FROM logs WHERE id = 3);

INSERT INTO logs (id, action, details)
SELECT 4,
       'CREATE_PRODUCT',
       'Product #4 added to inventory'
WHERE NOT EXISTS (SELECT 1 FROM logs WHERE id = 4);

INSERT INTO logs (id, action, details)
SELECT 5,
       'UPDATE_TASK',
       'Task #1 status changed to in_progress'
WHERE NOT EXISTS (SELECT 1 FROM logs WHERE id = 5);

INSERT INTO logs (id, action, details)
SELECT 6,
       'USER_LOGIN',
       'User #2 logged in'
WHERE NOT EXISTS (SELECT 1 FROM logs WHERE id = 6);

INSERT INTO logs (id, action, details)
SELECT 7,
       'USER_LOGIN',
       'User #3 logged in'
WHERE NOT EXISTS (SELECT 1 FROM logs WHERE id = 7);

INSERT INTO logs (id, action, details)
SELECT 8,
       'PRODUCT_UPDATE',
       'Product #2 stock updated'
WHERE NOT EXISTS (SELECT 1 FROM logs WHERE id = 8);

INSERT INTO logs (id, action, details)
SELECT 9,
       'TASK_COMPLETED',
       'Task #6 marked as completed'
WHERE NOT EXISTS (SELECT 1 FROM logs WHERE id = 9);

INSERT INTO logs (id, action, details)
SELECT 10,
       'ARCHIVE_TASK',
       'Task #12 archived'
WHERE NOT EXISTS (SELECT 1 FROM logs WHERE id = 10);


-- ============================================================
-- 5. CART TABLE
-- ============================================================

CREATE TABLE IF NOT EXISTS cart (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    product_name TEXT NOT NULL,
    price REAL NOT NULL,
    quantity INTEGER DEFAULT 1,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

INSERT INTO cart (id, product_name, price, quantity)
SELECT 1,
       'Mechanical Keyboard',
       89.99,
       1
WHERE NOT EXISTS (SELECT 1 FROM cart WHERE id = 1);

INSERT INTO cart (id, product_name, price, quantity)
SELECT 2,
       'Ergonomic Mouse',
       49.50,
       2
WHERE NOT EXISTS (SELECT 1 FROM cart WHERE id = 2);

INSERT INTO cart (id, product_name, price, quantity)
SELECT 3,
       'Curved Monitor 34"',
       399.00,
       1
WHERE NOT EXISTS (SELECT 1 FROM cart WHERE id = 3);

INSERT INTO cart (id, product_name, price, quantity)
SELECT 4,
       'USB-C Hub',
       39.99,
       2
WHERE NOT EXISTS (SELECT 1 FROM cart WHERE id = 4);

INSERT INTO cart (id, product_name, price, quantity)
SELECT 5,
       'Laptop Stand',
       59.95,
       1
WHERE NOT EXISTS (SELECT 1 FROM cart WHERE id = 5);

INSERT INTO cart (id, product_name, price, quantity)
SELECT 6,
       'Webcam HD',
       69.00,
       1
WHERE NOT EXISTS (SELECT 1 FROM cart WHERE id = 6);

INSERT INTO cart (id, product_name, price, quantity)
SELECT 7,
       'Desk Mat',
       24.99,
       3
WHERE NOT EXISTS (SELECT 1 FROM cart WHERE id = 7);

INSERT INTO cart (id, product_name, price, quantity)
SELECT 8,
       'Bluetooth Speaker',
       79.50,
       1
WHERE NOT EXISTS (SELECT 1 FROM cart WHERE id = 8);

INSERT INTO cart (id, product_name, price, quantity)
SELECT 9,
       'Noise Cancelling Headphones',
       149.99,
       1
WHERE NOT EXISTS (SELECT 1 FROM cart WHERE id = 9);

INSERT INTO cart (id, product_name, price, quantity)
SELECT 10,
       'Portable SSD 1TB',
       109.99,
       2
WHERE NOT EXISTS (SELECT 1 FROM cart WHERE id = 10);


-- ============================================================
-- VERIFY SEED DATA
-- ============================================================

SELECT 'users' AS table_name, COUNT(*) AS row_count FROM users
UNION ALL
SELECT 'products', COUNT(*) FROM products
UNION ALL
SELECT 'tasks', COUNT(*) FROM tasks
UNION ALL
SELECT 'logs', COUNT(*) FROM logs
UNION ALL
SELECT 'cart', COUNT(*) FROM cart;
