unit Lint4dFixture.Enums;

interface

type
  TColor = (clRed, clGreen, clBlue, clYellow, clWhite, clBlack);

  TColors = set of TColor;

  TDirection = (dirNorth, dirSouth, dirEast, dirWest);

const
  DEFAULT_COLOR = clBlue;
  MAX_COLORS = 6;
  APP_NAME = 'Lint4dFixtures';

implementation

end.
