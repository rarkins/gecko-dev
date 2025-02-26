# Compatibility Dataset

## How to update the MDN compatibility data
The Compatibility panel detects issues by comparing against official [MDN compatibility data](https://github.com/mdn/browser-compat-data). It uses a local snapshot of the dataset. This dataset needs to be manually synchronized periodically to `devtools/shared/compatibility/dataset` (ideally with every Firefox release).

The subsets from the dataset required by the Compatibility panel are:
* browsers: [https://github.com/mdn/browser-compat-data/tree/master/browsers](https://github.com/mdn/browser-compat-data/tree/master/browsers)
* css.properties: [https://github.com/mdn/browser-compat-data/tree/master/css](https://github.com/mdn/browser-compat-data/tree/master/css).

The MDN compatibility data is available as a node package ([@mdn/browser-compat-data](https://www.npmjs.com/package/@mdn/browser-compat-data)).
The following node program is a sample of how to download `browsers.json` and `css-properties.json` using the node package.

```javascript
'use strict';

const compatData = require("@mdn/browser-compat-data")
const fs = require("fs")
const path = require("path")

function exportData(data, fileName) {
  const content = `${ JSON.stringify(data) }`

  fs.writeFile(
    path.resolve(
      __dirname,
      fileName
    ),
    content,
    err => {
      if (err) {
        console.error(err)
      }
    }
  )
}

exportData(compatData.css.properties, "css-properties.json");
exportData(compatData.browsers, "browsers.json");

```

Save the JSON files created by the script to `devtools/shared/compatibility/dataset/`.

Check that all tests still pass. It is possible that changes in the structure or contents of the latest dataset will cause tests to fail. If that is the case, fix the tests. **Do not manually change the contents or structure of the local dataset** because any changes will be overwritten by the next update from the official dataset.
