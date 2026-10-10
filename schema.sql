-- ============================================================
-- VLO FRAMEWORK CRUD TESTING SCHEMA & SEED DUMP
-- Database Target: SQLite (vlo_app)
-- ============================================================

--CREATE DATABASE IF NOT EXISTS vlo_app;
--USE vlo_apps;

PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;

-- ============================================================
-- 1. USERS TABLE
-- ============================================================

CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    email TEXT UNIQUE NOT NULL,
    password_hash TEXT NOT NULL DEFAULT '',
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
-- 2. CUSTOMER TABLE
-- ============================================================

   CREATE TABLE IF NOT EXISTS customers (
       customer_id INTEGER PRIMARY KEY AUTOINCREMENT,
       full_name TEXT NOT NULL,
       email_address TEXT UNIQUE NOT NULL,
       pass_hash TEXT NOT NULL DEFAULT '',
       tier TEXT DEFAULT 'User'
   );
   INSERT INTO customers (full_name, email_address, tier)
   SELECT 'Test Customer', 'cust@shop.com', 'User'
   WHERE NOT EXISTS (SELECT 1 FROM customers WHERE email_address='cust@shop.com');
-- ============================================================
-- 2. SESSION TABLE
-- ============================================================

CREATE TABLE IF NOT EXISTS sessions (
    token TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS auth_tokens (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL,
    token TEXT UNIQUE NOT NULL,
    type TEXT NOT NULL, -- 'activate' or 'password_reset'
    expires_at INTEGER NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);
CREATE INDEX idx_auth_tokens_token ON auth_tokens(token);
CREATE INDEX idx_auth_tokens_user_id ON auth_tokens(user_id);
-- ============================================================
-- 2. PRODUCTS TABLE
-- ============================================================

CREATE TABLE products (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    description TEXT,
    category TEXT,
    price REAL NOT NULL,
    stock INTEGER DEFAULT 0,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

INSERT OR IGNORE INTO products (id,title,description,category,price,stock) VALUES
(11,'Gaming Mouse','High-precision RGB gaming mouse','Gaming',39.99,35),
(12,'Mechanical Keyboard TKL','Compact mechanical gaming keyboard','Gaming',74.99,22),
(13,'Wireless Keyboard','Slim wireless keyboard with USB receiver','Accessories',34.99,28),
(14,'Wireless Mouse','Silent wireless optical mouse','Accessories',24.99,45),
(15,'Gaming Headset','7.1 surround gaming headset','Gaming',59.99,18),
(16,'USB Microphone','Condenser microphone with USB connection','Electronics',89.99,14),
(17,'Monitor Arm','Adjustable single monitor arm','Home',49.99,20),
(18,'LED Desk Lamp','Dimmable LED desk lamp','Home',32.50,30),
(19,'Power Strip','6-port surge protected power strip','Home',29.99,40),
(20,'HDMI Cable','High-speed HDMI 2.1 cable','Accessories',14.99,60),
(21,'DisplayPort Cable','High-speed DisplayPort cable','Accessories',16.99,50),
(22,'USB-C Cable','Braided USB-C fast charging cable','Accessories',12.99,75),
(23,'USB-C Charger','65W GaN USB-C wall charger','Electronics',44.99,32),
(24,'Wireless Charger','15W Qi wireless charging pad','Electronics',27.99,38),
(25,'Laptop Backpack','Water-resistant laptop backpack','Accessories',54.99,25),
(26,'Laptop Sleeve','Protective 15-inch laptop sleeve','Accessories',29.99,42),
(27,'Webcam Cover','Sliding privacy webcam cover','Accessories',6.99,100),
(28,'USB Flash Drive 128GB','High-speed USB 3.2 flash drive','Accessories',18.99,65),
(29,'USB Flash Drive 256GB','High-speed USB 3.2 flash drive','Accessories',29.99,50),
(30,'External HDD 2TB','Portable 2TB USB external hard drive','Electronics',74.99,16),
(31,'External SSD 2TB','Portable USB-C 2TB solid state drive','Electronics',169.99,12),
(32,'WiFi Router','Dual-band AC wireless router','Electronics',69.99,15),
(33,'WiFi Extender','Dual-band wireless range extender','Electronics',39.99,24),
(34,'Bluetooth Adapter','USB Bluetooth 5.3 adapter','Accessories',11.99,55),
(35,'USB Hub','4-port USB 3.0 hub','Accessories',19.99,48),
(36,'USB-C Hub Pro','10-in-1 USB-C docking hub','Accessories',79.99,17),
(37,'Laptop Cooling Pad','Dual-fan laptop cooling pad','Gaming',36.99,26),
(38,'Gaming Desk','Large RGB gaming desk','Gaming',179.99,9),
(39,'Gaming Chair','Ergonomic adjustable gaming chair','Gaming',249.99,7),
(40,'Office Chair','Ergonomic mesh office chair','Home',189.99,11),
(41,'Foot Rest','Adjustable under-desk foot rest','Home',29.99,34),
(42,'Monitor Stand','Wooden dual-level monitor stand','Home',39.99,23),
(43,'Cable Organizer','Desktop cable management kit','Accessories',15.99,70),
(44,'Desk Organizer','Multi-compartment desk organizer','Home',22.99,31),
(45,'Desk Shelf','Minimal wooden desk shelf','Home',64.99,19),
(46,'Smart Plug','WiFi smart plug with app control','Electronics',17.99,44),
(47,'Smart Bulb','RGB WiFi smart LED bulb','Electronics',21.99,39),
(48,'LED Light Strip','5-meter RGB LED light strip','Home',25.99,27),
(49,'Power Bank 10000mAh','Compact fast charging power bank','Electronics',29.99,33),
(50,'Power Bank 20000mAh','High-capacity fast charging power bank','Electronics',44.99,21),
(51,'Travel Adapter','Universal international travel adapter','Accessories',31.99,18),
(52,'Surge Protector','8-outlet surge protection strip','Home',34.99,29),
(53,'Desk Fan','Quiet USB desktop fan','Home',19.99,36),
(54,'Portable Fan','Rechargeable portable mini fan','Home',16.99,41),
(55,'Bluetooth Earbuds','True wireless Bluetooth earbuds','Electronics',49.99,25),
(56,'Sports Earbuds','Wireless sweat-resistant earbuds','Electronics',54.99,18),
(57,'Soundbar','Compact Bluetooth desktop soundbar','Electronics',89.99,13),
(58,'Portable Speaker Mini','Compact waterproof Bluetooth speaker','Electronics',39.99,20),
(59,'Portable Speaker Pro','Powerful portable Bluetooth speaker','Electronics',119.99,8),
(60,'Smartwatch','Fitness smartwatch with heart-rate tracking','Electronics',99.99,14),
(61,'Fitness Tracker','Activity and sleep fitness tracker','Electronics',59.99,22),
(62,'USB Desk Clock','Digital USB desk clock','Home',24.99,28),
(63,'Alarm Clock','Digital alarm clock with LED display','Home',21.99,35),
(64,'Phone Stand','Adjustable aluminum phone stand','Accessories',17.99,52),
(65,'Tablet Stand','Adjustable tablet desktop stand','Accessories',22.99,38),
(66,'Phone Tripod','Compact smartphone tripod','Accessories',34.99,24),
(67,'Ring Light','10-inch adjustable LED ring light','Electronics',39.99,17),
(68,'Action Camera','4K waterproof action camera','Electronics',129.99,10),
(69,'Camera Tripod','Lightweight adjustable camera tripod','Accessories',44.99,16),
(70,'SD Card 128GB','High-speed 128GB microSD card','Accessories',17.99,70),
(71,'SD Card 256GB','High-speed 256GB microSD card','Accessories',27.99,55),
(72,'Card Reader','USB-C multi-format card reader','Accessories',18.99,42),
(73,'E-Reader','6-inch glare-free e-reader','Electronics',129.99,9),
(74,'Tablet 10-inch','10-inch Full HD Android tablet','Electronics',179.99,12),
(75,'Tablet Case','Protective tablet folio case','Accessories',24.99,45),
(76,'Laptop Stand Pro','Premium adjustable laptop stand','Home',79.99,15),
(77,'Laptop Dock','USB-C laptop docking station','Electronics',139.99,11),
(78,'Mini PC','Compact desktop mini PC','Electronics',399.99,6),
(79,'Desktop PC','Performance desktop computer','Gaming',899.99,5),
(80,'Gaming Monitor','27-inch 165Hz gaming monitor','Gaming',279.99,8),
(81,'4K Monitor','27-inch 4K productivity monitor','Electronics',329.99,7),
(82,'Portable Monitor','15.6-inch USB-C portable monitor','Electronics',219.99,10),
(83,'Monitor Light Bar','USB monitor LED light bar','Home',49.99,24),
(84,'Keyboard Wrist Rest','Memory foam keyboard wrist rest','Accessories',19.99,40),
(85,'Mouse Pad XL','Extended RGB gaming mouse pad','Gaming',29.99,37),
(86,'Mouse Bungee','Gaming mouse cable management bungee','Gaming',18.99,29),
(87,'Controller Stand','Dual controller desktop stand','Gaming',21.99,26),
(88,'Wireless Controller','Wireless PC gaming controller','Gaming',49.99,19),
(89,'Gaming Keypad','Compact programmable gaming keypad','Gaming',69.99,13),
(90,'Streaming Deck','Programmable streaming control deck','Gaming',119.99,8),
(91,'Capture Card','USB 1080p video capture card','Gaming',89.99,12),
(92,'Green Screen','Portable collapsible green screen','Gaming',99.99,7),
(93,'Desk Microphone Arm','Adjustable microphone boom arm','Gaming',39.99,22),
(94,'Pop Filter','Microphone pop filter','Accessories',12.99,45),
(95,'Shock Mount','Universal microphone shock mount','Accessories',24.99,30),
(96,'Studio Headphones','Closed-back studio monitoring headphones','Electronics',109.99,14),
(97,'DAC Headphone Amp','USB desktop headphone amplifier','Electronics',79.99,11),
(98,'Bluetooth Transmitter','Bluetooth audio transmitter','Electronics',27.99,25),
(99,'Smart Display','Compact smart home display','Electronics',89.99,13),
(100,'Smart Speaker','Voice-enabled smart speaker','Electronics',69.99,17),
(101,'Robot Vacuum','Smart robotic vacuum cleaner','Home',299.99,6),
(102,'Air Purifier','HEPA desktop air purifier','Home',129.99,10),
(103,'Humidifier','Ultrasonic room humidifier','Home',49.99,18),
(104,'Electric Kettle','Fast-boil stainless steel kettle','Home',44.99,23),
(105,'Coffee Maker','Programmable drip coffee maker','Home',79.99,14),
(106,'Blender','High-speed countertop blender','Home',69.99,16),
(107,'Digital Scale','Precision kitchen digital scale','Home',19.99,31),
(108,'Smart Scale','Bluetooth body composition scale','Electronics',39.99,20),
(109,'Electric Toothbrush','Rechargeable electric toothbrush','Electronics',59.99,18),
(110,'Hair Dryer','Compact ionic hair dryer','Electronics',49.99,21),
(111,'Power Bank Slim','Slim 5000mAh power bank','Electronics',19.99,40),
(112,'Charging Station','Multi-device charging station','Electronics',49.99,26),
(113,'Wireless Charging Stand','Vertical wireless phone charger','Electronics',34.99,32),
(114,'Car Charger','Dual-port USB-C car charger','Accessories',19.99,48),
(115,'Car Phone Mount','Magnetic dashboard phone mount','Accessories',22.99,36),
(116,'Dash Camera','1080p car dash camera','Electronics',79.99,13),
(117,'Bluetooth Car Adapter','Wireless Bluetooth car audio adapter','Electronics',24.99,29),
(118,'Portable Jump Starter','Compact car battery jump starter','Electronics',89.99,9),
(119,'LED Flashlight','Rechargeable tactical LED flashlight','Accessories',24.99,33),
(120,'Headlamp','Rechargeable LED headlamp','Accessories',21.99,38),
(121,'Camping Lantern','Rechargeable outdoor camping lantern','Home',34.99,25),
(122,'Travel Mug','Insulated stainless steel travel mug','Home',27.99,42),
(123,'Water Bottle','Insulated stainless steel water bottle','Home',24.99,50),
(124,'Lunch Box','Insulated portable lunch box','Home',19.99,34),
(125,'Backpack','Everyday water-resistant backpack','Accessories',49.99,27),
(126,'Travel Backpack','Large multi-compartment travel backpack','Accessories',79.99,16),
(127,'Duffel Bag','Lightweight travel duffel bag','Accessories',44.99,23),
(128,'Luggage Scale','Portable digital luggage scale','Accessories',14.99,44),
(129,'Travel Organizer','Electronic accessories travel organizer','Accessories',19.99,52),
(130,'Cable Bag','Compact cable storage pouch','Accessories',17.99,61),
(131,'Keyboard Cover','Universal laptop keyboard cover','Accessories',9.99,80),
(132,'Screen Protector','Laptop anti-glare screen protector','Accessories',19.99,47),
(133,'Laptop Privacy Filter','Magnetic laptop privacy screen','Accessories',39.99,22),
(134,'Cleaning Kit','Electronics cleaning kit','Accessories',14.99,75),
(135,'Compressed Air Duster','Rechargeable electronic air duster','Electronics',49.99,19),
(136,'Mini Vacuum','USB rechargeable desktop vacuum','Home',29.99,31),
(137,'Cable Tester','USB and network cable tester','Electronics',24.99,20),
(138,'Network Switch','8-port Gigabit Ethernet switch','Electronics',39.99,18),
(139,'Ethernet Cable','Cat6 Ethernet cable 10m','Accessories',14.99,55),
(140,'WiFi USB Adapter','Dual-band USB WiFi adapter','Electronics',22.99,37),
(141,'Mesh WiFi System','Dual-node mesh WiFi system','Electronics',179.99,7),
(142,'Smart Doorbell','WiFi video smart doorbell','Electronics',119.99,11),
(143,'Security Camera','1080p indoor WiFi security camera','Electronics',49.99,24),
(144,'Outdoor Camera','Weather-resistant outdoor security camera','Electronics',89.99,15),
(145,'Smart Lock','Bluetooth smart door lock','Electronics',159.99,8),
(146,'Smart Sensor','Wireless motion sensor','Electronics',19.99,35),
(147,'Smart Thermostat','WiFi programmable smart thermostat','Electronics',129.99,6),
(148,'Smart Light Switch','WiFi smart wall switch','Electronics',29.99,27),
(149,'Smart LED Panel','Modular RGB LED wall panels','Home',99.99,14),
(150,'LED Strip Pro','10-meter addressable RGB LED strip','Home',59.99,18),
(151,'Gaming Chair Pro','Premium reclining gaming chair','Gaming',329.99,5),
(152,'Gaming Desk Pro','Large electric height-adjustable desk','Gaming',349.99,6),
(153,'Gaming Mouse Pro','Ultra-light wireless gaming mouse','Gaming',99.99,12),
(154,'Gaming Keyboard Pro','Wireless mechanical gaming keyboard','Gaming',139.99,9),
(155,'Gaming Headset Pro','Wireless surround gaming headset','Gaming',129.99,11),
(156,'Gaming Controller Pro','Premium wireless gaming controller','Gaming',89.99,15),
(157,'Gaming Mouse Pad','Premium stitched gaming mouse pad','Gaming',34.99,32),
(158,'Gaming Speakers','RGB desktop gaming speakers','Gaming',69.99,17),
(159,'Gaming Router','Low-latency gaming WiFi router','Gaming',199.99,7),
(160,'Gaming Monitor 4K','32-inch 4K 144Hz gaming monitor','Gaming',499.99,5),
(161,'Graphics Tablet','Medium wireless drawing tablet','Electronics',149.99,10),
(162,'Stylus Pen','Pressure-sensitive tablet stylus','Accessories',59.99,21),
(163,'Drawing Display','13-inch pen display tablet','Electronics',299.99,6),
(164,'Laser Printer','Compact monochrome laser printer','Electronics',159.99,8),
(165,'Inkjet Printer','Wireless color inkjet printer','Electronics',99.99,13),
(166,'Document Scanner','Compact duplex document scanner','Electronics',179.99,7),
(167,'Label Printer','Bluetooth thermal label printer','Electronics',69.99,18),
(168,'Thermal Paper','Roll pack for thermal printers','Accessories',12.99,100),
(169,'Barcode Scanner','USB handheld barcode scanner','Electronics',54.99,20),
(170,'USB Numeric Keypad','Compact external numeric keypad','Accessories',17.99,34),
(171,'Ergonomic Keyboard','Split ergonomic office keyboard','Home',89.99,16),
(172,'Vertical Mouse','Ergonomic vertical wireless mouse','Home',44.99,25),
(173,'Ergonomic Footrest','Memory foam ergonomic footrest','Home',34.99,28),
(174,'Standing Desk Mat','Anti-fatigue standing desk mat','Home',39.99,22),
(175,'Desk Drawer','Under-desk storage drawer','Home',29.99,19),
(176,'Monitor Riser','Adjustable monitor riser shelf','Home',34.99,27),
(177,'Whiteboard','Magnetic desktop whiteboard','Home',24.99,30),
(178,'Desk Calendar','Minimal desktop calendar','Home',14.99,45),
(179,'Notebook','Premium hardcover notebook','Home',12.99,80),
(180,'Pen Set','Premium metal ballpoint pen set','Accessories',19.99,55),
(181,'Smart Pen','Digital note-taking smart pen','Electronics',79.99,14),
(182,'Portable Projector','Mini Full HD portable projector','Electronics',229.99,8),
(183,'Projector Screen','Portable projector screen','Home',69.99,13),
(184,'HDMI Splitter','4K HDMI splitter','Accessories',29.99,31),
(185,'HDMI Switch','4-port 4K HDMI switch','Accessories',34.99,26),
(186,'USB-C to HDMI Adapter','4K USB-C display adapter','Accessories',24.99,40),
(187,'Ethernet Adapter','USB-C Gigabit Ethernet adapter','Accessories',27.99,29),
(188,'Thunderbolt Cable','40Gbps Thunderbolt cable','Accessories',49.99,17),
(189,'Thunderbolt Dock','Premium Thunderbolt docking station','Electronics',249.99,7),
(190,'Portable SSD 4TB','High-speed 4TB portable SSD','Electronics',329.99,6),
(191,'NAS Storage','Two-bay network attached storage','Electronics',299.99,5),
(192,'NAS Hard Drive 4TB','NAS optimized 4TB hard drive','Electronics',109.99,12),
(193,'Memory Card 512GB','High-speed 512GB microSD card','Accessories',49.99,36),
(194,'Memory Card 1TB','High-capacity 1TB microSD card','Accessories',109.99,15),
(195,'USB DVD Drive','External USB DVD writer','Accessories',29.99,21),
(196,'Blu-ray Drive','External USB Blu-ray drive','Electronics',89.99,8),
(197,'Gamepad Stand','RGB desktop gamepad stand','Gaming',24.99,28),
(198,'VR Headset','Standalone virtual reality headset','Gaming',399.99,5),
(199,'VR Controller','Replacement VR motion controller','Gaming',99.99,9),
(200,'VR Cable','High-speed VR link cable','Gaming',39.99,18),
(201,'Gaming Desk Mat','Large waterproof gaming desk mat','Gaming',39.99,31),
(202,'RGB Light Bar','Dual RGB monitor light bars','Gaming',59.99,15),
(203,'RGB Keyboard Wrist Rest','RGB memory foam wrist rest','Gaming',29.99,26),
(204,'Gaming Chair Mat','Hard floor chair protection mat','Gaming',49.99,19),
(205,'Controller Charging Dock','Dual controller charging dock','Gaming',34.99,23),
(206,'Console Headset','Wireless console gaming headset','Gaming',79.99,14),
(207,'Console Stand','Vertical console cooling stand','Gaming',44.99,12),
(208,'Game Storage Rack','Multi-slot game storage rack','Gaming',39.99,20),
(209,'Gaming Capture Pro','4K HDMI capture card','Gaming',159.99,7),
(210,'Streaming Microphone','USB condenser streaming microphone','Gaming',119.99,10),
(211,'Streaming Light','Adjustable RGB streaming light','Gaming',69.99,16),
(212,'Streaming Camera','4K USB streaming webcam','Gaming',149.99,9),
(213,'Podcast Headphones','Closed-back podcast headphones','Electronics',79.99,17),
(214,'Podcast Mixer','Compact USB audio mixer','Electronics',129.99,8),
(215,'Audio Interface','Two-channel USB audio interface','Electronics',139.99,11),
(216,'Studio Monitor','Compact powered studio monitor pair','Electronics',199.99,6),
(217,'MIDI Keyboard','49-key USB MIDI controller','Electronics',119.99,12),
(218,'USB MIDI Cable','USB MIDI connection cable','Accessories',17.99,35),
(219,'Smart Plug Pro','Energy monitoring smart plug','Electronics',24.99,33),
(220,'Smart Bulb Pro','Color-changing smart LED bulb','Electronics',29.99,42),
(221,'Smart Light Panel','WiFi RGB smart light panel','Home',79.99,14),
(222,'Smart Curtain Controller','WiFi curtain automation controller','Electronics',69.99,9),
(223,'Smart Motion Sensor','Wireless smart motion detector','Electronics',27.99,25),
(224,'Smart Temperature Sensor','WiFi temperature and humidity sensor','Electronics',34.99,21),
(225,'Smart Water Leak Sensor','Wireless water leak detector','Home',29.99,28),
(226,'Smart Smoke Detector','Connected smoke and heat detector','Home',59.99,13),
(227,'Air Quality Monitor','Indoor air quality monitor','Electronics',89.99,10),
(228,'Digital Thermometer','Fast digital room thermometer','Home',14.99,45),
(229,'Desk Humidifier','USB mini desktop humidifier','Home',24.99,37),
(230,'Desk Air Purifier','Compact HEPA desktop purifier','Home',69.99,16),
(231,'Portable Projector Mini','Pocket-size portable projector','Electronics',159.99,9),
(232,'Projector Tripod','Adjustable projector tripod stand','Accessories',39.99,18),
(233,'HDMI Cable 10m','Long high-speed HDMI cable','Accessories',29.99,34),
(234,'USB-C Cable 2m','Long braided USB-C charging cable','Accessories',15.99,62),
(235,'USB-C Cable 3m','Extra-long USB-C data cable','Accessories',19.99,48),
(236,'GaN Charger 100W','100W multi-port GaN charger','Electronics',79.99,18),
(237,'GaN Charger 140W','140W high-power USB-C charger','Electronics',109.99,11),
(238,'Charging Cable Kit','Multi-connector charging cable kit','Accessories',24.99,43),
(239,'Power Strip USB-C','Power strip with USB-C charging','Home',39.99,25),
(240,'Travel Power Strip','Compact travel charging station','Accessories',34.99,31),
(241,'Wireless Presenter','Wireless presentation clicker','Accessories',29.99,27),
(242,'Laser Presenter','Presentation clicker with laser pointer','Accessories',39.99,22),
(243,'Document Camera','USB document presentation camera','Electronics',99.99,12),
(244,'USB Light','Adjustable USB reading light','Home',14.99,55),
(245,'Reading Lamp','Rechargeable adjustable reading lamp','Home',29.99,34),
(246,'Desk Clock Pro','Smart digital desk clock','Electronics',39.99,24),
(247,'Bluetooth Alarm Clock','Bluetooth speaker alarm clock','Electronics',49.99,19),
(248,'Smart Plug Mini','Compact WiFi smart plug','Electronics',14.99,47),
(249,'Universal Remote','Universal smart home remote','Electronics',29.99,28),
(250,'Smart Home Hub','Central smart home automation hub','Electronics',99.99,10);

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
