unit SuppressedFix;

interface

type
  // lint4d:ignore type-prefix
  MyClass = class(TObject)
  end;

const
  maxSize = 100; // lint4d:ignore constant-naming

implementation

end.
